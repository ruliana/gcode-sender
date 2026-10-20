//! The controller actor. It owns the serial port and is the only place that
//! talks to the machine, so every job transition is serialized here.
//!
//! Streaming uses GRBL character counting: the actor tracks how many bytes
//! of sent lines have not been acknowledged yet and only sends the next line
//! when it fits in the controller's RX buffer. Every line sent, whether part
//! of a job or internal, goes through the same in-flight queue, because the
//! controller acknowledges them all in order with `ok` or `error:N`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_util::codec::{Decoder, FramedRead};
use tracing::{debug, info, warn};

use crate::grbl::{self, FirmwareKind, Response, Status, realtime};
use crate::program::ProgramLine;
use crate::store::{JobRecord, JobState, JobStore, LineError, now_ms};

#[derive(Clone, Debug)]
pub struct Config {
    pub port: String,
    pub baud: u32,
    /// Bytes of the RX buffer left unused as a safety margin.
    pub rx_reserve: usize,
    pub poll_interval: Duration,
    pub reconnect_interval: Duration,
    /// How long to wait for a feed hold to settle before resetting on stop.
    pub stop_timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Connection {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Clone, Debug, Serialize)]
pub struct Firmware {
    pub kind: FirmwareKind,
    pub version: String,
}

/// What `GET /machine` reports. Published on every change.
#[derive(Clone, Debug, Serialize)]
pub struct MachineSnapshot {
    pub connection: Connection,
    pub port: String,
    /// True once the firmware has been identified and jobs can start.
    pub ready: bool,
    pub firmware: Option<Firmware>,
    pub rx_buffer_size: Option<usize>,
    pub status: Option<Status>,
    pub alarm: Option<u32>,
    pub last_error: Option<String>,
    /// The freshest record of the active job, if any.
    pub active_job: Option<JobRecord>,
}

#[derive(Debug)]
pub enum ControlError {
    NotFound,
    Conflict(String),
    Unavailable(String),
    Invalid(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ControlError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

type Reply<T> = oneshot::Sender<Result<T, ControlError>>;

enum Command {
    Start(String, Reply<JobRecord>),
    Pause(String, Reply<JobRecord>),
    Resume(String, Reply<JobRecord>),
    Stop(String, Reply<JobRecord>),
    Delete(String, Reply<()>),
}

#[derive(Clone)]
pub struct ControllerHandle {
    tx: mpsc::Sender<Command>,
    snapshot: watch::Receiver<MachineSnapshot>,
}

impl ControllerHandle {
    pub fn snapshot(&self) -> MachineSnapshot {
        self.snapshot.borrow().clone()
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, ControlError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| ControlError::Unavailable("controller is not running".into()))?;
        rx.await
            .map_err(|_| ControlError::Unavailable("controller is not running".into()))?
    }

    pub async fn start(&self, id: String) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Start(id, r)).await
    }
    pub async fn pause(&self, id: String) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Pause(id, r)).await
    }
    pub async fn resume(&self, id: String) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Resume(id, r)).await
    }
    pub async fn stop(&self, id: String) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Stop(id, r)).await
    }
    pub async fn delete(&self, id: String) -> Result<(), ControlError> {
        self.call(|r| Command::Delete(id, r)).await
    }
}

pub fn spawn(config: Config, store: Arc<JobStore>) -> ControllerHandle {
    let (tx, rx) = mpsc::channel(32);
    let snapshot = MachineSnapshot {
        connection: Connection::Connecting,
        port: config.port.clone(),
        ready: false,
        firmware: None,
        rx_buffer_size: None,
        status: None,
        alarm: None,
        last_error: None,
        active_job: None,
    };
    let (snap_tx, snap_rx) = watch::channel(snapshot.clone());
    let actor = Actor {
        config,
        store,
        commands: rx,
        snapshot_tx: snap_tx,
        machine: snapshot,
        job: None,
    };
    tokio::spawn(actor.run());
    ControllerHandle {
        tx,
        snapshot: snap_rx,
    }
}

/// Splits serial input into lines, tolerating the garbage bytes a board
/// can print while it resets.
struct LineDecoder;

impl Decoder for LineDecoder {
    type Item = String;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<String>, Self::Error> {
        let Some(end) = src.iter().position(|&b| b == b'\n') else {
            if src.len() > 4096 {
                src.clear();
            }
            return Ok(None);
        };
        let line = src.split_to(end + 1);
        Ok(Some(String::from_utf8_lossy(&line).trim().to_string()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// Index into the active job's lines.
    Job(usize),
    /// The `$I` build info query used to identify the firmware.
    Info,
}

struct InFlight {
    origin: Origin,
    bytes: usize,
}

struct ActiveJob {
    record: JobRecord,
    lines: Vec<ProgramLine>,
    next: usize,
    stop_requested: Option<Instant>,
    dirty: bool,
    last_saved: Instant,
}

impl ActiveJob {
    fn all_acked(&self) -> bool {
        self.record.acked_lines >= self.lines.len()
    }
}

/// Per-connection state. Dropped when the port closes.
struct Session {
    writer: WriteHalf<SerialStream>,
    inflight: VecDeque<InFlight>,
    inflight_bytes: usize,
    rx_buffer: usize,
    info: InfoProbe,
    /// Set after a soft reset is sent; acknowledgements are meaningless
    /// until the controller prints its banner again.
    awaiting_reset: Option<Instant>,
}

#[derive(Default)]
struct InfoProbe {
    attempts: u32,
    next_at: Option<Instant>,
    kind: Option<FirmwareKind>,
    version: Option<String>,
    rx_buffer: Option<usize>,
}

struct Actor {
    config: Config,
    store: Arc<JobStore>,
    commands: mpsc::Receiver<Command>,
    snapshot_tx: watch::Sender<MachineSnapshot>,
    machine: MachineSnapshot,
    job: Option<ActiveJob>,
}

/// Grbl 1.1 ships with a 128 byte RX buffer, the smallest in the family.
const DEFAULT_RX_BUFFER: usize = 128;
const INFO_RETRY: Duration = Duration::from_secs(1);
const RESET_TIMEOUT: Duration = Duration::from_secs(3);
const SAVE_INTERVAL: Duration = Duration::from_millis(500);

impl Actor {
    async fn run(mut self) {
        loop {
            self.machine.connection = Connection::Connecting;
            self.publish();
            match tokio_serial::new(&self.config.port, self.config.baud).open_native_async() {
                Ok(port) => {
                    info!(port = %self.config.port, "serial port opened");
                    self.session(port).await;
                }
                Err(e) => {
                    debug!(port = %self.config.port, error = %e, "cannot open serial port");
                    self.machine.last_error = Some(format!("cannot open serial port: {e}"));
                }
            }
            self.disconnected();
            let wait = tokio::time::sleep(self.config.reconnect_interval);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    cmd = self.commands.recv() => match cmd {
                        Some(cmd) => self.handle_offline(cmd),
                        None => return,
                    },
                }
            }
        }
    }

    fn disconnected(&mut self) {
        self.machine.connection = Connection::Disconnected;
        self.machine.ready = false;
        self.machine.status = None;
        if self.job.is_some() {
            self.finish_job(
                JobState::Interrupted,
                Some("the serial connection was lost".into()),
            );
        }
        self.publish();
    }

    fn handle_offline(&mut self, cmd: Command) {
        match cmd {
            Command::Start(_, r) => {
                let _ = r.send(Err(ControlError::Unavailable(
                    "the machine is not connected".into(),
                )));
            }
            Command::Pause(id, r) | Command::Resume(id, r) | Command::Stop(id, r) => {
                let _ = r.send(self.inactive_error(&id));
            }
            Command::Delete(id, r) => {
                let _ = r.send(self.delete(&id));
            }
        }
    }

    fn inactive_error(&self, id: &str) -> Result<JobRecord, ControlError> {
        match self.store.get(id)? {
            None => Err(ControlError::NotFound),
            Some(job) => Err(ControlError::Conflict(format!(
                "job is {}, not active",
                state_name(job.state)
            ))),
        }
    }

    fn delete(&mut self, id: &str) -> Result<(), ControlError> {
        if self.job.as_ref().is_some_and(|j| j.record.id == id) {
            return Err(ControlError::Conflict("job is active; stop it first".into()));
        }
        if self.store.delete(id)? {
            Ok(())
        } else {
            Err(ControlError::NotFound)
        }
    }

    async fn session(&mut self, port: SerialStream) {
        let (reader, writer) = tokio::io::split(port);
        let mut lines = FramedRead::new(reader, LineDecoder);
        let mut s = Session {
            writer,
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            rx_buffer: DEFAULT_RX_BUFFER,
            info: InfoProbe::default(),
            awaiting_reset: None,
        };
        self.machine.connection = Connection::Connected;
        self.machine.last_error = None;
        self.publish();

        let mut poll = tokio::time::interval(self.config.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let result = tokio::select! {
                line = lines.next() => match line {
                    Some(Ok(line)) => self.on_line(&mut s, &line).await,
                    Some(Err(e)) => Err(e),
                    None => Err(std::io::ErrorKind::UnexpectedEof.into()),
                },
                cmd = self.commands.recv() => match cmd {
                    Some(cmd) => self.on_command(&mut s, cmd).await,
                    None => return,
                },
                _ = poll.tick() => self.on_tick(&mut s).await,
            };
            if let Err(e) = result {
                warn!(error = %e, "serial connection lost");
                self.machine.last_error = Some(format!("serial connection lost: {e}"));
                return;
            }
            self.save_progress(false);
            self.publish();
        }
    }

    async fn on_tick(&mut self, s: &mut Session) -> std::io::Result<()> {
        s.writer.write_all(&[realtime::STATUS]).await?;
        let now = Instant::now();

        if !self.machine.ready
            && s.awaiting_reset.is_none()
            && !s.inflight.iter().any(|f| f.origin == Origin::Info)
            && s.info.next_at.is_none_or(|t| now >= t)
        {
            // Boards that reset when the port opens swallow anything sent
            // during boot, so keep asking until one probe is answered.
            s.info.attempts += 1;
            s.info.next_at = Some(now + INFO_RETRY);
            debug!(attempt = s.info.attempts, "querying build info");
            self.send_line(s, "$I", Origin::Info).await?;
        }

        if let Some(sent) = s.awaiting_reset
            && sent.elapsed() > RESET_TIMEOUT
        {
            warn!("no banner after soft reset");
            self.after_reset(s);
        }

        let stop_overdue = self
            .job
            .as_ref()
            .and_then(|j| j.stop_requested)
            .is_some_and(|t| t.elapsed() > self.config.stop_timeout);
        if stop_overdue && s.awaiting_reset.is_none() {
            warn!("feed hold did not settle in time; resetting anyway");
            self.soft_reset(s).await?;
        }
        Ok(())
    }

    async fn on_line(&mut self, s: &mut Session, line: &str) -> std::io::Result<()> {
        if line.is_empty() {
            return Ok(());
        }
        debug!(%line, "rx");
        match grbl::parse(line) {
            Response::Ok => self.on_ack(s, None).await?,
            Response::Error(code) => self.on_ack(s, Some(code)).await?,
            Response::Alarm(code) => {
                warn!(code, "alarm");
                self.machine.alarm = Some(code);
                if self.job.as_ref().is_some_and(|j| j.stop_requested.is_none()) {
                    self.finish_job(JobState::Failed, Some(format!("controller alarm {code}")));
                }
            }
            Response::Banner { kind, version } => {
                info!(?kind, %version, "controller started");
                s.info = InfoProbe {
                    kind: Some(kind),
                    version: Some(version),
                    ..InfoProbe::default()
                };
                self.machine.ready = false;
                if s.awaiting_reset.is_some() {
                    self.after_reset(s);
                } else {
                    s.inflight.clear();
                    s.inflight_bytes = 0;
                    if self.job.is_some() {
                        self.finish_job(
                            JobState::Failed,
                            Some("the controller reset during the job".into()),
                        );
                    }
                }
            }
            Response::Status(mut status) => {
                if let Some(wco) = status.work_offset.clone() {
                    self.machine
                        .status
                        .get_or_insert_with(Status::default)
                        .work_offset = Some(wco);
                }
                let wco = self
                    .machine
                    .status
                    .as_ref()
                    .and_then(|st| st.work_offset.clone());
                if status.work_position.is_none()
                    && let (Some(m), Some(o)) = (&status.machine_position, &wco)
                {
                    status.work_position = Some(m.iter().zip(o).map(|(m, o)| m - o).collect());
                }
                status.work_offset = wco;
                if status.state != "Alarm" {
                    self.machine.alarm = None;
                }
                self.on_status(s, &status).await?;
                self.machine.status = Some(status);
            }
            Response::Message { tag, body } => match tag.as_str() {
                "VER" => {
                    let version = body.split(':').next().unwrap_or_default();
                    let version = version.split('.').take(2).collect::<Vec<_>>().join(".");
                    s.info.version.get_or_insert(version);
                }
                "OPT" => s.info.rx_buffer = grbl::rx_buffer_from_opt(&body),
                "FIRMWARE" if body.eq_ignore_ascii_case("grblhal") => {
                    s.info.kind = Some(FirmwareKind::Grblhal)
                }
                _ => {}
            },
            Response::Other(_) => {}
        }
        Ok(())
    }

    async fn on_status(&mut self, s: &mut Session, status: &Status) -> std::io::Result<()> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        if job.stop_requested.is_some() {
            if s.awaiting_reset.is_none() && (status.hold_complete() || status.state == "Idle") {
                self.soft_reset(s).await?;
            }
        } else if job.record.state == JobState::Running
            && job.all_acked()
            && status.state == "Idle"
        {
            // Every line is in the planner and the machine has come to rest.
            self.finish_job(JobState::Completed, None);
        }
        Ok(())
    }

    async fn on_ack(&mut self, s: &mut Session, error: Option<u32>) -> std::io::Result<()> {
        if s.awaiting_reset.is_some() {
            return Ok(());
        }
        let Some(done) = s.inflight.pop_front() else {
            debug!("acknowledgement with nothing in flight");
            return Ok(());
        };
        s.inflight_bytes -= done.bytes;
        match done.origin {
            Origin::Info => match error {
                None => {
                    let kind = s.info.kind.unwrap_or(FirmwareKind::Grbl);
                    s.rx_buffer = s.info.rx_buffer.unwrap_or(DEFAULT_RX_BUFFER);
                    self.machine.firmware = Some(Firmware {
                        kind,
                        version: s.info.version.clone().unwrap_or_default(),
                    });
                    self.machine.rx_buffer_size = Some(s.rx_buffer);
                    self.machine.ready = true;
                    info!(?kind, rx_buffer = s.rx_buffer, "controller identified");
                }
                // Busy (error:8) or locked; the next tick asks again.
                Some(code) => debug!(code, "build info query refused"),
            },
            Origin::Job(index) => {
                if let Some(job) = &mut self.job {
                    job.record.acked_lines += 1;
                    let line = &job.lines[index];
                    job.record.last_acked_line = Some(line.number);
                    job.dirty = true;
                    if let Some(code) = error {
                        warn!(line = line.number, code, text = %line.text, "line rejected");
                        job.record.line_errors.push(LineError {
                            line: line.number,
                            code,
                            text: line.text.clone(),
                        });
                        if job.record.state == JobState::Running {
                            // Hold so the operator can decide whether the
                            // rest of the program is still safe to run.
                            s.writer.write_all(&[realtime::FEED_HOLD]).await?;
                            job.record.state = JobState::Paused;
                            job.record.reason =
                                Some(format!("line {} was rejected with error {code}", line.number));
                            self.save_progress(true);
                        }
                    }
                }
            }
        }
        self.pump(s).await
    }

    async fn on_command(&mut self, s: &mut Session, cmd: Command) -> std::io::Result<()> {
        match cmd {
            Command::Start(id, reply) => {
                let result = self.start(s, &id);
                let ok = result.is_ok();
                let _ = reply.send(result);
                if ok {
                    self.pump(s).await?;
                }
            }
            Command::Pause(id, reply) => {
                let result = self.check_active(&id, &[JobState::Running]);
                if result.is_ok() {
                    s.writer.write_all(&[realtime::FEED_HOLD]).await?;
                    self.transition(JobState::Paused, Some("paused on request".into()));
                }
                let _ = reply.send(result.map(|_| self.active_record()));
            }
            Command::Resume(id, reply) => {
                let result = self.check_active(&id, &[JobState::Paused]);
                if result.is_ok() {
                    s.writer.write_all(&[realtime::CYCLE_START]).await?;
                    self.transition(JobState::Running, None);
                }
                let _ = reply.send(result.map(|_| self.active_record()));
                self.pump(s).await?;
            }
            Command::Stop(id, reply) => {
                let result = self.check_active(&id, &[JobState::Running, JobState::Paused]);
                if result.is_ok() {
                    // Hold first so the machine decelerates instead of
                    // losing position; the reset follows on Hold:0.
                    s.writer.write_all(&[realtime::FEED_HOLD]).await?;
                    if let Some(job) = &mut self.job {
                        job.stop_requested = Some(Instant::now());
                    }
                    self.transition(JobState::Stopping, Some("stopped on request".into()));
                }
                let _ = reply.send(result.map(|_| self.active_record()));
            }
            Command::Delete(id, reply) => {
                let _ = reply.send(self.delete(&id));
            }
        }
        Ok(())
    }

    fn start(&mut self, s: &Session, id: &str) -> Result<JobRecord, ControlError> {
        let Some(mut record) = self.store.get(id)? else {
            return Err(ControlError::NotFound);
        };
        if let Some(active) = &self.job {
            return Err(ControlError::Conflict(format!(
                "job {} is already active",
                active.record.id
            )));
        }
        if !self.machine.ready {
            return Err(ControlError::Unavailable(
                "the controller has not been identified yet".into(),
            ));
        }
        if let Some(code) = self.machine.alarm {
            return Err(ControlError::Conflict(format!(
                "the machine is in alarm {code}"
            )));
        }
        if let Some(state) = self.machine.status.as_ref().map(|st| st.state.as_str())
            && state != "Idle"
        {
            return Err(ControlError::Conflict(format!(
                "the machine is {state}, not Idle"
            )));
        }
        let lines = self.store.program(id)?;
        if lines.is_empty() {
            return Err(ControlError::Invalid("the program has no lines to send".into()));
        }
        let capacity = self.capacity(s);
        if let Some(long) = lines.iter().find(|l| l.text.len() + 1 > capacity) {
            return Err(ControlError::Invalid(format!(
                "line {} is longer than the controller's buffer",
                long.number
            )));
        }
        record.state = JobState::Running;
        record.total_lines = lines.len();
        record.acked_lines = 0;
        record.last_acked_line = None;
        record.started_at_ms = Some(now_ms());
        record.finished_at_ms = None;
        record.reason = None;
        record.line_errors.clear();
        self.store.save(&record)?;
        info!(id = %record.id, lines = lines.len(), "job started");
        self.job = Some(ActiveJob {
            record: record.clone(),
            lines,
            next: 0,
            stop_requested: None,
            dirty: false,
            last_saved: Instant::now(),
        });
        Ok(record)
    }

    fn check_active(&self, id: &str, allowed: &[JobState]) -> Result<(), ControlError> {
        match &self.job {
            Some(job) if job.record.id == id => {
                if allowed.contains(&job.record.state) {
                    Ok(())
                } else {
                    Err(ControlError::Conflict(format!(
                        "job is {}",
                        state_name(job.record.state)
                    )))
                }
            }
            _ => self.inactive_error(id).map(|_| ()),
        }
    }

    fn active_record(&self) -> JobRecord {
        self.job.as_ref().map(|j| j.record.clone()).expect("active job")
    }

    fn capacity(&self, s: &Session) -> usize {
        s.rx_buffer.saturating_sub(self.config.rx_reserve).max(1)
    }

    /// Sends job lines while they fit in the controller's RX buffer.
    async fn pump(&mut self, s: &mut Session) -> std::io::Result<()> {
        if s.awaiting_reset.is_some() {
            return Ok(());
        }
        let capacity = self.capacity(s);
        let Some(job) = &mut self.job else {
            return Ok(());
        };
        if job.record.state != JobState::Running {
            return Ok(());
        }
        let mut out = Vec::new();
        while let Some(line) = job.lines.get(job.next) {
            let bytes = line.text.len() + 1;
            if s.inflight_bytes + bytes > capacity {
                break;
            }
            out.extend_from_slice(line.text.as_bytes());
            out.push(b'\n');
            s.inflight.push_back(InFlight {
                origin: Origin::Job(job.next),
                bytes,
            });
            s.inflight_bytes += bytes;
            job.next += 1;
        }
        if !out.is_empty() {
            s.writer.write_all(&out).await?;
        }
        Ok(())
    }

    async fn send_line(&mut self, s: &mut Session, text: &str, origin: Origin) -> std::io::Result<()> {
        let bytes = text.len() + 1;
        s.writer.write_all(format!("{text}\n").as_bytes()).await?;
        s.inflight.push_back(InFlight { origin, bytes });
        s.inflight_bytes += bytes;
        Ok(())
    }

    async fn soft_reset(&mut self, s: &mut Session) -> std::io::Result<()> {
        s.writer.write_all(&[realtime::SOFT_RESET]).await?;
        s.awaiting_reset = Some(Instant::now());
        Ok(())
    }

    /// The controller has reset after a stop: nothing sent before survives.
    fn after_reset(&mut self, s: &mut Session) {
        s.awaiting_reset = None;
        s.inflight.clear();
        s.inflight_bytes = 0;
        s.info.next_at = None;
        if self.job.as_ref().is_some_and(|j| j.stop_requested.is_some()) {
            self.finish_job(JobState::Stopped, Some("stopped on request".into()));
        }
    }

    fn transition(&mut self, state: JobState, reason: Option<String>) {
        if let Some(job) = &mut self.job {
            job.record.state = state;
            job.record.reason = reason;
            self.save_progress(true);
        }
    }

    fn finish_job(&mut self, state: JobState, reason: Option<String>) {
        let Some(mut job) = self.job.take() else {
            return;
        };
        info!(id = %job.record.id, ?state, ?reason, "job finished");
        job.record.state = state;
        job.record.finished_at_ms = Some(now_ms());
        if reason.is_some() {
            job.record.reason = reason;
        }
        if let Err(e) = self.store.save(&job.record) {
            warn!(error = %e, "cannot save job record");
        }
    }

    fn save_progress(&mut self, force: bool) {
        let Some(job) = &mut self.job else {
            return;
        };
        if !(force || job.dirty && job.last_saved.elapsed() >= SAVE_INTERVAL) {
            return;
        }
        if let Err(e) = self.store.save(&job.record) {
            warn!(error = %e, "cannot save job progress");
        }
        job.dirty = false;
        job.last_saved = Instant::now();
    }

    fn publish(&mut self) {
        self.machine.active_job = self.job.as_ref().map(|j| j.record.clone());
        self.snapshot_tx.send_replace(self.machine.clone());
    }
}

pub fn state_name(state: JobState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}
