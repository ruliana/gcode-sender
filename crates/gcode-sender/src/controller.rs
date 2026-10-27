//! The controller actor. It owns the serial port and is the only place that
//! talks to the machine, so every job transition is serialized here.
//!
//! Streaming uses GRBL character counting: the actor tracks how many bytes
//! of sent lines have not been acknowledged yet and only sends the next line
//! when it fits in the controller's RX buffer. Every line sent, whether part
//! of a job or internal, goes through the same in-flight queue, because the
//! controller acknowledges them all in order with `ok` or `error:N`.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_util::codec::{Decoder, FramedRead};
use tracing::{debug, info, warn};

use crate::bounds::{self, Bounds};
use crate::gcode::{self, Interpreter, Modal};
use crate::grbl::{self, FirmwareKind, Response, Status, realtime};
use crate::position::{self, Check, Position, Record};
use crate::program::ProgramLine;
use crate::settings::{PauseOptions, PauseSettings};
use crate::store::{JobRecord, JobState, JobStore, LineError, PausePoint, now_ms};
use crate::work::{self, WorkOffsets, Zeros};

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
    /// How long the controller may say nothing, not even answer a status
    /// query, before the connection is treated as dead.
    pub silence_timeout: Duration,
    /// How long lines may sit unacknowledged while the machine is idle
    /// before the acknowledgement is considered lost.
    pub ack_timeout: Duration,
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
    /// True once the firmware has been identified and the work coordinate
    /// systems read, so jobs can start.
    pub ready: bool,
    pub firmware: Option<Firmware>,
    pub rx_buffer_size: Option<usize>,
    pub status: Option<Status>,
    /// The work coordinate systems G54 to G59: which one is active, and
    /// where each zero is. Read from the controller while it is idle.
    pub work: Option<WorkOffsets>,
    pub alarm: Option<u32>,
    pub alarm_message: Option<String>,
    pub last_error: Option<String>,
    /// The freshest record of the active job, if any.
    pub active_job: Option<JobRecord>,
    /// A recorded position the controller disagrees with, awaiting an answer.
    pub position_check: Option<Check>,
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
    Pause(String, PauseOptions, Reply<JobRecord>),
    Resume(String, ResumeOptions, Reply<JobRecord>),
    Stop(String, Reply<JobRecord>),
    PositionCheck(bool, Reply<()>),
    Delete(String, Reply<()>),
    Machine(MachineAction, Reply<()>),
    Jog(JogRequest, Reply<()>),
    Work(WorkChange, AxisValues, Reply<()>),
}

/// What to do differently when resuming a job that is not active.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeOptions {
    /// Restart at this file line, or the first line of code after it,
    /// instead of backing up from the saved progress.
    pub from_line: Option<usize>,
    /// Lift the tool before moving back to the work.
    pub retract: Option<Retract>,
    /// Resume even though the work zero (G54) is not where it was when the
    /// job last ran, which moves the whole cut.
    #[serde(default)]
    pub accept_moved_zero: bool,
}

/// Lift the tool, travel above the resume point, and plunge back to it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retract {
    /// Safe Z in machine coordinates, in millimetres. It must not be below
    /// the tool's current Z or the resume point's Z.
    pub z: f64,
    /// Plunge feed in millimetres per minute. Defaults to the program's
    /// feed rate at the resume point.
    pub plunge_feed: Option<f64>,
    /// Seconds to wait for the spindle to reach speed before plunging.
    /// Keep it below `--ack-timeout-ms`.
    pub dwell_s: Option<f64>,
}

/// Things an operator can ask of the machine outside a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineAction {
    /// `$X`: clear an alarm lock without homing.
    Unlock,
    /// `$H`: run the homing cycle.
    Home,
    /// Soft reset, which also clears a feed hold left by an earlier session.
    Reset,
    /// Realtime jog cancel: stop a jog in progress and drop the queued ones.
    JogCancel,
}

/// One number per axis, and the system they apply to. Axes left out are
/// not touched.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisValues {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub z: Option<f64>,
    /// The work coordinate system, `G54` to `G59`. Defaults to the active
    /// one. `PUT /machine/work-system` takes only this.
    pub system: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkChange {
    /// `G10 L20`: the tool is here, and here has these work coordinates.
    Zero,
    /// `G10 L2`: work coordinates start at these machine coordinates.
    Offset,
    /// Makes a system the active one, which jogs and `G54`-style moves use.
    Select,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JogMode {
    /// Move by the given distances from where the tool is.
    #[default]
    Relative,
    /// Move to the given coordinates.
    Absolute,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JogSpace {
    /// Coordinates of the active work coordinate system.
    #[default]
    Work,
    /// Coordinates measured from the machine's home.
    Machine,
}

/// A short, interruptible move for positioning the tool by hand. Distances
/// and feed are in millimetres and millimetres per minute.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JogRequest {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub z: Option<f64>,
    /// Left out, the feed comes from the saved jog speeds: `jog.xy_feed` for
    /// X and Y, `jog.z_feed` for Z, and the slower of the two for a move that
    /// uses both.
    pub feed: Option<f64>,
    #[serde(default)]
    pub mode: JogMode,
    /// Only for absolute jogs. Defaults to the work coordinate system.
    pub space: Option<JogSpace>,
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

    /// A receiver that wakes whenever the snapshot is published.
    pub fn subscribe(&self) -> watch::Receiver<MachineSnapshot> {
        self.snapshot.clone()
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
    pub async fn pause(
        &self,
        id: String,
        options: PauseOptions,
    ) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Pause(id, options, r)).await
    }
    pub async fn resume(
        &self,
        id: String,
        options: ResumeOptions,
    ) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Resume(id, options, r)).await
    }
    pub async fn stop(&self, id: String) -> Result<JobRecord, ControlError> {
        self.call(|r| Command::Stop(id, r)).await
    }
    pub async fn delete(&self, id: String) -> Result<(), ControlError> {
        self.call(|r| Command::Delete(id, r)).await
    }
    pub async fn machine(&self, action: MachineAction) -> Result<(), ControlError> {
        self.call(|r| Command::Machine(action, r)).await
    }
    /// Resolves once the controller has accepted or refused the change.
    pub async fn work(&self, change: WorkChange, values: AxisValues) -> Result<(), ControlError> {
        self.call(|r| Command::Work(change, values, r)).await
    }
    /// Resolves once the controller has accepted or refused the jog.
    pub async fn jog(&self, request: JogRequest) -> Result<(), ControlError> {
        self.call(|r| Command::Jog(request, r)).await
    }
    /// Answers a position check: `moved` says whether the machine has moved
    /// since the sender last recorded the tool's position.
    pub async fn position_check(&self, moved: bool) -> Result<(), ControlError> {
        self.call(|r| Command::PositionCheck(moved, r)).await
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
        work: None,
        alarm: None,
        alarm_message: None,
        last_error: None,
        active_job: None,
        position_check: None,
    };
    let position = store.position().unwrap_or_else(|e| {
        warn!(error = %e, "cannot read the recorded tool position; starting without one");
        Position::default()
    });
    let (snap_tx, snap_rx) = watch::channel(snapshot.clone());
    let actor = Actor {
        config,
        store,
        commands: rx,
        snapshot_tx: snap_tx,
        machine: snapshot,
        job: None,
        jog_target: None,
        position,
        pending: None,
        last_position_save: Instant::now(),
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

/// The replies to `$#` and `$G`, collected until both are in.
#[derive(Default)]
struct ModesProbe {
    zeros: [Option<[f64; 3]>; 6],
    system: Option<u32>,
}

impl ModesProbe {
    /// The work coordinate systems, once every zero and the active system
    /// have been reported.
    fn complete(&self) -> Option<WorkOffsets> {
        let mut zeros = [[0.0; 3]; 6];
        for (slot, zero) in zeros.iter_mut().zip(self.zeros) {
            *slot = zero?;
        }
        Some(WorkOffsets {
            system: self.system?,
            zeros,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// Index into the active job's lines.
    Job(usize),
    /// The `$I` build info query used to identify the firmware.
    Info,
    /// A line that restores modal state before a resumed job continues.
    Preamble,
    /// An operator command such as `$X`.
    Internal,
    /// A `$J=` jog; the caller waits for the controller's answer.
    Jog,
    /// A `G10` that sets the work zero; the caller waits for the answer.
    Work,
    /// The jog that lifts the tool when a pause frees the machine.
    Lift,
    /// A `G10` that moves the work zero to match the recorded position.
    Sync,
    /// The `$#` and `$G` queries that read the work coordinate systems.
    Modes,
}

struct InFlight {
    origin: Origin,
    bytes: usize,
    /// Told what the controller answered, for commands someone waits on.
    reply: Option<Reply<()>>,
}

/// Forgets what was sent. Anyone waiting on an answer is told the line may
/// not have been executed.
fn clear_inflight(s: &mut Session) {
    for f in s.inflight.drain(..) {
        if let Some(reply) = f.reply {
            let _ = reply.send(Err(ControlError::Unavailable(
                "the controller reset or went silent before it answered".into(),
            )));
        }
    }
    s.inflight_bytes = 0;
}

struct ActiveJob {
    record: JobRecord,
    lines: Vec<ProgramLine>,
    next: usize,
    /// Lines that rebuild modal state before `next`, sent first on resume.
    preamble: VecDeque<String>,
    stop_requested: Option<Instant>,
    /// A resume was requested; cycle start is sent once the hold has
    /// settled, because the controller ignores it while decelerating.
    resume_pending: bool,
    /// Set while a pause is stopping the machine and lifting the tool, after
    /// which the job is released and the machine is free to be moved.
    lift_pause: Option<LiftPause>,
    dirty: bool,
    last_saved: Instant,
}

struct LiftPause {
    settings: PauseSettings,
    /// Where the tool stopped, in machine coordinates.
    position: Option<[f64; 3]>,
    /// The lift jog has been sent.
    lifting: bool,
    /// The controller accepted the lift jog; the pause ends when it is done.
    lift_acked: bool,
    lifted_to: Option<f64>,
    /// Why the tool was not lifted, if it was not.
    note: Option<String>,
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
    planner_blocks: usize,
    /// When anything last arrived from the controller.
    last_rx: Instant,
    /// When the controller last acknowledged a line, or when the first line
    /// of the current batch went out.
    last_ack: Instant,
    /// Set after a soft reset is sent; acknowledgements are meaningless
    /// until the controller prints its banner again.
    awaiting_reset: Option<Instant>,
    /// The reported position has been compared with the recorded one on this
    /// connection.
    reconciled: bool,
    /// Set once the firmware has been identified.
    identified: bool,
    /// When the work coordinate systems were last asked for.
    modes_read_at: Instant,
    /// What `$#` and `$G` have reported so far, until a full set is read.
    modes: ModesProbe,
    /// Set when the work coordinate systems must be read again.
    modes_stale: bool,
}

#[derive(Default)]
struct InfoProbe {
    attempts: u32,
    next_at: Option<Instant>,
    kind: Option<FirmwareKind>,
    version: Option<String>,
    rx_buffer: Option<usize>,
    planner_blocks: Option<usize>,
    /// The build info reply has started. The controller may still be
    /// answering lines an earlier session left in its buffer, and those
    /// acknowledgements arrive first; only an `ok` that follows the `[OPT:`
    /// line belongs to the probe.
    saw_opt: bool,
}

struct Actor {
    config: Config,
    store: Arc<JobStore>,
    commands: mpsc::Receiver<Command>,
    snapshot_tx: watch::Sender<MachineSnapshot>,
    machine: MachineSnapshot,
    job: Option<ActiveJob>,
    /// Where the queued jogs will leave the tool, in machine coordinates.
    jog_target: Option<([f64; 3], Instant)>,
    /// The recorded tool position and any check awaiting an answer.
    position: Position,
    /// A change to the position waiting for the controller to accept the
    /// work zeros that go with it.
    pending: Option<Pending>,
    last_position_save: Instant,
}

/// A change waiting for the controller to accept its work zeros, one line
/// per system.
struct Pending {
    change: Change,
    outstanding: usize,
}

/// A change to the recorded position, applied once any zeros it needs have
/// been accepted by the controller.
struct Change {
    /// The zeros to move from and to, if they move.
    zeros: Option<(Zeros, Zeros)>,
    /// How far the machine coordinates of paused jobs move with the frame.
    shift: [f64; 3],
    record: Record,
    check: Option<Check>,
}

/// Grbl 1.1 ships with a 128 byte RX buffer, the smallest in the family.
const DEFAULT_RX_BUFFER: usize = 128;
/// Grbl 1.1 plans 15 blocks (16 slots), the smallest in the family.
const DEFAULT_PLANNER_BLOCKS: usize = 16;
const INFO_RETRY: Duration = Duration::from_secs(1);
const RESET_TIMEOUT: Duration = Duration::from_secs(3);
const SAVE_INTERVAL: Duration = Duration::from_millis(250);
/// How often the work coordinate systems are read again while idle.
const MODES_REFRESH: Duration = Duration::from_secs(2);
/// Feed for the lift jog. The controller caps it at the axis's maximum rate.
const LIFT_FEED: f64 = 2000.0;
/// Plunge feed on resume when the program has no plain feed rate to use.
const FALLBACK_PLUNGE_FEED: f64 = 300.0;

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
        self.machine.work = None;
        self.machine.status = None;
        if self.job.is_some() {
            let reason = self
                .machine
                .last_error
                .clone()
                .unwrap_or_else(|| "the serial connection was lost".into());
            self.finish_job(JobState::Interrupted, Some(reason));
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
            Command::Resume(id, _, r) if self.is_resumable(&id) => {
                let _ = r.send(Err(ControlError::Unavailable(
                    "the machine is not connected".into(),
                )));
            }
            Command::Pause(id, _, r) | Command::Resume(id, _, r) => {
                let _ = r.send(self.inactive_error(&id));
            }
            Command::Stop(id, r) => {
                let _ = r.send(self.stop_released(&id));
            }
            Command::Delete(id, r) => {
                let _ = r.send(self.delete(&id));
            }
            Command::Machine(_, r)
            | Command::Jog(_, r)
            | Command::Work(_, _, r)
            | Command::PositionCheck(_, r) => {
                let _ = r.send(Err(ControlError::Unavailable(
                    "the machine is not connected".into(),
                )));
            }
        }
    }

    /// A job that ended before it finished and can be continued, or one
    /// whose pause freed the machine.
    fn is_resumable(&self, id: &str) -> bool {
        self.job.is_none()
            && matches!(
                self.store.get(id),
                Ok(Some(j)) if matches!(
                    j.state,
                    JobState::Interrupted | JobState::Stopped | JobState::Failed
                ) || j.state == JobState::Paused && j.paused_at.is_some()
            )
    }

    /// Stopping a paused job that already freed the machine only has to
    /// retire the job, since there is nothing running.
    fn stop_released(&mut self, id: &str) -> Result<JobRecord, ControlError> {
        if self.job.is_none()
            && let Some(mut record) = self.store.get(id)?
            && record.state == JobState::Paused
            && record.paused_at.is_some()
        {
            record.state = JobState::Stopped;
            record.reason = Some("stopped on request".into());
            record.paused_at = None;
            record.finished_at_ms = Some(now_ms());
            self.store.save(&record)?;
            return Ok(record);
        }
        self.inactive_error(id)
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
            return Err(ControlError::Conflict(
                "job is active; stop it first".into(),
            ));
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
            planner_blocks: DEFAULT_PLANNER_BLOCKS,
            last_rx: Instant::now(),
            last_ack: Instant::now(),
            awaiting_reset: None,
            reconciled: false,
            identified: false,
            modes_read_at: Instant::now(),
            modes: ModesProbe::default(),
            modes_stale: true,
        };
        self.machine.connection = Connection::Connected;
        self.machine.last_error = None;
        self.publish();

        let mut poll = tokio::time::interval(self.config.poll_interval);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let result = tokio::select! {
                line = lines.next() => match line {
                    Some(Ok(line)) => {
                        s.last_rx = Instant::now();
                        self.on_line(&mut s, &line).await
                    }
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
                self.pending = None;
                return;
            }
            self.save_progress(false);
            self.publish();
        }
    }

    async fn on_tick(&mut self, s: &mut Session) -> std::io::Result<()> {
        s.writer.write_all(&[realtime::STATUS]).await?;
        let now = Instant::now();

        // The controller answers a status query even in the middle of a
        // move, so total silence means the link is dead, not busy.
        if s.last_rx.elapsed() > self.config.silence_timeout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the controller stopped responding",
            ));
        }

        // Lines normally stay unacknowledged only while the planner is full,
        // and then the machine is moving. Idle with lines outstanding means
        // an acknowledgement was lost, and nobody knows what the controller
        // has received.
        let idle = self
            .machine
            .status
            .as_ref()
            .is_some_and(|st| st.state == "Idle");
        if !s.inflight.is_empty()
            && idle
            && s.awaiting_reset.is_none()
            && s.last_ack.elapsed() > self.config.ack_timeout
        {
            warn!(outstanding = s.inflight.len(), "acknowledgement lost");
            if self.job.is_some() {
                self.finish_job(
                    JobState::Failed,
                    Some("the controller stopped acknowledging lines".into()),
                );
                self.machine.last_error = Some("an acknowledgement was lost".into());
                self.soft_reset(s).await?;
            } else {
                self.forget_inflight(s);
            }
        }

        if !s.identified
            && s.awaiting_reset.is_none()
            && !s.inflight.iter().any(|f| f.origin == Origin::Info)
            && s.info.next_at.is_none_or(|t| now >= t)
        {
            // Boards that reset when the port opens swallow anything sent
            // during boot, so keep asking until one probe is answered.
            s.info.attempts += 1;
            s.info.saw_opt = false;
            s.info.next_at = Some(now + INFO_RETRY);
            debug!(attempt = s.info.attempts, "querying build info");
            self.send_line(s, "$I", Origin::Info).await?;
        }

        // Zeros can also change behind the server's back, from a panel or
        // another client, so they are read again every so often while idle.
        if s.identified
            && self.job.is_none()
            && s.awaiting_reset.is_none()
            && !s.inflight.iter().any(|f| f.origin == Origin::Modes)
            && (s.modes_stale || now.duration_since(s.modes_read_at) >= MODES_REFRESH)
        {
            self.read_modes(s).await?;
        }

        if let Some(sent) = s.awaiting_reset
            && sent.elapsed() > RESET_TIMEOUT
        {
            warn!("no banner after soft reset");
            self.after_reset(s).await?;
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
                self.machine.alarm_message = Some(grbl::alarm_message(code).into());
                // An alarm throws away what the controller had buffered, so
                // the lines in flight will never be acknowledged. Forget them
                // now, or the next job would be counted against them.
                self.forget_inflight(s);
                if self
                    .job
                    .as_ref()
                    .is_some_and(|j| j.stop_requested.is_none())
                {
                    self.finish_job(
                        JobState::Failed,
                        Some(format!(
                            "controller alarm {code}: {}",
                            grbl::alarm_message(code)
                        )),
                    );
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
                s.identified = false;
                s.modes_stale = true;
                self.machine.work = None;
                if s.awaiting_reset.is_some() {
                    self.after_reset(s).await?;
                } else {
                    self.forget_inflight(s);
                    self.jog_target = None;
                    if self.job.is_some() {
                        self.finish_job(
                            JobState::Interrupted,
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
                // The report gives the active system's zero, so it is always current.
                if let (Some(work), Some(zero)) = (
                    self.machine.work.as_mut(),
                    three(status.work_offset.as_ref()),
                ) {
                    work.zeros[work::index(work.system).unwrap_or_default()] = zero;
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
                    self.machine.alarm_message = None;
                }
                self.on_status(s, &status).await?;
                if s.reconciled {
                    self.record_position(&status);
                } else if self.machine.ready
                    && self.machine.work.is_some()
                    && status.state == "Idle"
                {
                    self.reconcile(s, &status).await?;
                }
                self.machine.status = Some(status);
            }
            Response::Message { tag, body } => match tag.as_str() {
                "VER" => {
                    let version = body.split(':').next().unwrap_or_default();
                    let version = version.split('.').take(2).collect::<Vec<_>>().join(".");
                    s.info.version.get_or_insert(version);
                }
                "OPT" => {
                    s.info.saw_opt = true;
                    s.info.rx_buffer = grbl::rx_buffer_from_opt(&body);
                    s.info.planner_blocks = grbl::planner_blocks_from_opt(&body);
                }
                "FIRMWARE" if body.eq_ignore_ascii_case("grblhal") => {
                    s.info.kind = Some(FirmwareKind::Grblhal)
                }
                "GC" => s.modes.system = grbl::active_system_message(&body),
                _ => {
                    if let Some((i, zero)) = grbl::work_zero_message(&tag, &body) {
                        s.modes.zeros[i] = Some(zero);
                    }
                }
            },
            Response::Other(_) => {}
        }
        Ok(())
    }

    async fn on_status(&mut self, s: &mut Session, status: &Status) -> std::io::Result<()> {
        let Some(job) = &mut self.job else {
            return Ok(());
        };
        if job.resume_pending {
            if status.hold_complete() {
                s.writer.write_all(&[realtime::CYCLE_START]).await?;
            } else if status.state != "Hold" {
                job.resume_pending = false;
            }
        }
        if job.stop_requested.is_some() {
            if s.awaiting_reset.is_none() && (status.hold_complete() || status.state == "Idle") {
                if let Some(lift) = &mut job.lift_pause {
                    lift.position
                        .get_or_insert_with(|| machine_position(status).unwrap_or_default());
                }
                self.soft_reset(s).await?;
            }
        } else if job.record.state == JobState::Running && job.all_acked() && status.state == "Idle"
        {
            // Every line is in the planner and the machine has come to rest.
            self.finish_job(JobState::Completed, None);
        } else if job
            .lift_pause
            .as_ref()
            .is_some_and(|l| l.lift_acked && status.state == "Idle")
        {
            self.release_paused(None);
        }
        Ok(())
    }

    /// Asks for the work coordinate systems: `$#` for the zeros, `$G` for
    /// the active one. The answer is collected by `Origin::Modes`.
    async fn read_modes(&mut self, s: &mut Session) -> std::io::Result<()> {
        s.modes_stale = false;
        s.modes_read_at = Instant::now();
        s.modes = ModesProbe::default();
        self.send_line(s, "$#", Origin::Modes).await?;
        self.send_line(s, "$G", Origin::Modes).await
    }

    async fn on_ack(&mut self, s: &mut Session, error: Option<u32>) -> std::io::Result<()> {
        if s.awaiting_reset.is_some() {
            return Ok(());
        }
        if error.is_none()
            && s.inflight.front().is_some_and(|f| f.origin == Origin::Info)
            && !s.info.saw_opt
        {
            debug!("ignoring an acknowledgement that predates this session");
            return Ok(());
        }
        let Some(done) = s.inflight.pop_front() else {
            debug!("acknowledgement with nothing in flight");
            return Ok(());
        };
        s.inflight_bytes -= done.bytes;
        s.last_ack = Instant::now();
        match done.origin {
            Origin::Info => match error {
                None => {
                    let kind = s.info.kind.unwrap_or(FirmwareKind::Grbl);
                    s.rx_buffer = s.info.rx_buffer.unwrap_or(DEFAULT_RX_BUFFER);
                    s.planner_blocks = s.info.planner_blocks.unwrap_or(DEFAULT_PLANNER_BLOCKS);
                    self.machine.firmware = Some(Firmware {
                        kind,
                        version: s.info.version.clone().unwrap_or_default(),
                    });
                    self.machine.rx_buffer_size = Some(s.rx_buffer);
                    // Jobs wait for the work coordinate systems too, so read them now.
                    s.identified = true;
                    info!(?kind, rx_buffer = s.rx_buffer, "controller identified");
                    self.read_modes(s).await?;
                }
                // Busy (error:8) or locked; the next tick asks again.
                Some(code) => debug!(code, "build info query refused"),
            },
            Origin::Modes => {
                let batch_done = !s.inflight.iter().any(|f| f.origin == Origin::Modes);
                match (error, s.modes.complete()) {
                    (None, Some(work)) => {
                        self.machine.work = Some(work);
                        self.machine.ready = s.identified;
                    }
                    (None, None) if batch_done => s.modes_stale = true,
                    (None, None) => {}
                    (Some(code), _) => {
                        debug!(code, "work coordinate query refused");
                        s.modes_stale = true;
                    }
                }
            }
            Origin::Internal => {
                if let Some(code) = error {
                    warn!(code, "command rejected");
                    self.machine.last_error = Some(format!(
                        "command rejected: error {code}, {}",
                        grbl::error_message(code)
                    ));
                }
            }
            Origin::Jog => {
                if let Some(reply) = done.reply {
                    let _ = reply.send(match error {
                        None => Ok(()),
                        Some(code) => {
                            self.jog_target = None;
                            let message = format!(
                                "the controller rejected the jog: error {code}, {}",
                                grbl::error_message(code)
                            );
                            Err(match code {
                                8 | 9 => ControlError::Conflict(message),
                                _ => ControlError::Invalid(message),
                            })
                        }
                    });
                }
            }
            Origin::Lift => {
                if let Some(lift) = self.job.as_mut().and_then(|j| j.lift_pause.as_mut()) {
                    match error {
                        None => lift.lift_acked = true,
                        Some(code) => {
                            lift.lifted_to = None;
                            lift.note = Some(format!(
                                "the controller refused the lift: error {code}, {}",
                                grbl::error_message(code)
                            ));
                            self.release_paused(None);
                        }
                    }
                }
            }
            Origin::Sync => match error {
                None => {
                    let done = self.pending.as_mut().is_some_and(|p| {
                        p.outstanding = p.outstanding.saturating_sub(1);
                        p.outstanding == 0
                    });
                    if done && let Some(pending) = self.pending.take() {
                        self.commit(pending.change);
                    }
                }
                Some(code) => {
                    self.pending = None;
                    self.machine.last_error = Some(format!(
                        "the controller refused to move the work zeros: error {code}, {}",
                        grbl::error_message(code)
                    ));
                }
            },
            Origin::Work => {
                if let Some(reply) = done.reply {
                    let _ = reply.send(match error {
                        None => {
                            // The controller reports the new offset on its
                            // next status; until then the old one is wrong.
                            if let Some(st) = &mut self.machine.status {
                                st.work_offset = None;
                                st.work_position = None;
                            }
                            // Re-read the systems: the zero, or the active one, changed.
                            s.modes_stale = true;
                            self.machine.work = None;
                            Ok(())
                        }
                        Some(code) => {
                            let message = format!(
                                "the controller rejected the change: error {code}, {}",
                                grbl::error_message(code)
                            );
                            Err(match code {
                                8 | 9 => ControlError::Conflict(message),
                                _ => ControlError::Invalid(message),
                            })
                        }
                    });
                }
            }
            Origin::Preamble => {
                if let Some(code) = error {
                    // Without the right modal state, continuing would cut
                    // in the wrong units or at the wrong speed.
                    self.finish_job(
                        JobState::Failed,
                        Some(format!(
                            "the controller rejected the lines that restore its state on resume: error {code}, {}",
                            grbl::error_message(code)
                        )),
                    );
                }
            }
            Origin::Job(index) => {
                if let Some(job) = &mut self.job
                    && let Some(line) = job.lines.get(index).cloned()
                {
                    job.record.acked_lines += 1;
                    job.record.last_acked_line = Some(line.number);
                    job.dirty = true;
                    if let Some(code) = error {
                        warn!(line = line.number, code, text = %line.text, "line rejected");
                        job.record.line_errors.push(LineError {
                            line: line.number,
                            code,
                            message: grbl::error_message(code).into(),
                            text: line.text.clone(),
                        });
                        if job.record.state == JobState::Running {
                            // Hold so the operator can decide whether the
                            // rest of the program is still safe to run.
                            s.writer.write_all(&[realtime::FEED_HOLD]).await?;
                            job.record.state = JobState::Paused;
                            job.record.reason = Some(format!(
                                "line {} was rejected with error {code}: {}",
                                line.number,
                                grbl::error_message(code)
                            ));
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
            Command::Pause(id, options, reply) => {
                let mut result = self.check_active(&id, &[JobState::Running]);
                let mut lift = None;
                let mut skipped = None;
                if result.is_ok() {
                    match self.plan_lift(s, &options) {
                        Ok((settings, why)) => (lift, skipped) = (settings, why),
                        Err(e) => result = Err(e),
                    }
                }
                if result.is_ok() {
                    s.writer.write_all(&[realtime::FEED_HOLD]).await?;
                    if let Some(job) = &mut self.job {
                        job.resume_pending = false;
                        if let Some(settings) = lift {
                            // The hold settles, a reset clears the planner,
                            // and then the tool is lifted.
                            job.stop_requested = Some(Instant::now());
                            job.lift_pause = Some(LiftPause {
                                settings,
                                position: None,
                                lifting: false,
                                lift_acked: false,
                                lifted_to: None,
                                note: None,
                            });
                        }
                    }
                    let reason = match (&lift_planned(&self.job), skipped) {
                        (true, _) => "pausing: bringing the machine to rest, then lifting the tool"
                            .to_string(),
                        (false, Some(why)) => {
                            format!("paused on request; the tool was not lifted: {why}")
                        }
                        (false, None) => "paused on request".to_string(),
                    };
                    self.transition(JobState::Paused, Some(reason));
                }
                let _ = reply.send(result.map(|_| self.active_record()));
            }
            Command::Resume(id, options, reply) if self.is_resumable(&id) => {
                let result = self.resume_job(s, &id, &options);
                let ok = result.is_ok();
                let _ = reply.send(result);
                if ok {
                    self.pump(s).await?;
                }
            }
            Command::Machine(action, reply) => {
                let result = self.machine_action(s, action).await?;
                let _ = reply.send(result);
            }
            Command::Jog(request, reply) => self.jog(s, request, reply).await?,
            Command::Work(change, values, reply) => self.work(s, change, values, reply).await?,
            Command::Resume(id, options, reply) => {
                let mut result = self.check_active(&id, &[JobState::Paused]);
                if result.is_ok() && self.job.as_ref().is_some_and(|j| j.lift_pause.is_some()) {
                    result = Err(ControlError::Conflict(
                        "the pause is still bringing the machine to rest and lifting the tool; try again in a moment"
                            .into(),
                    ));
                }
                if result.is_ok() && (options.from_line.is_some() || options.retract.is_some()) {
                    result = Err(ControlError::Invalid(
                        "from_line and retract only apply to a job that is not active".into(),
                    ));
                }
                if result.is_ok() {
                    if let Some(job) = &mut self.job {
                        job.resume_pending = true;
                    }
                    self.transition(JobState::Running, None);
                }
                let _ = reply.send(result.map(|_| self.active_record()));
                self.pump(s).await?;
            }
            Command::Stop(id, reply) if !self.job.as_ref().is_some_and(|j| j.record.id == id) => {
                let _ = reply.send(self.stop_released(&id));
            }
            Command::Stop(id, reply) => {
                let result = self.check_active(&id, &[JobState::Running, JobState::Paused]);
                if result.is_ok() {
                    if let Some(job) = &mut self.job {
                        job.resume_pending = false;
                        // A stop outranks a pause that is still lifting.
                        job.lift_pause = None;
                        job.record.lift_pending = None;
                    }
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
            Command::PositionCheck(moved, reply) => {
                let result = self.answer_position_check(s, moved).await?;
                let _ = reply.send(result);
            }
        }
        Ok(())
    }

    /// Decides whether a pause lifts the tool. Returns the settings to lift
    /// with, or, when the pause has to be a plain hold, why.
    fn plan_lift(
        &self,
        s: &Session,
        options: &PauseOptions,
    ) -> Result<(Option<PauseSettings>, Option<String>), ControlError> {
        let settings = options
            .apply(&self.store.settings()?.pause)
            .map_err(ControlError::Invalid)?;
        if !settings.lift {
            return Ok((None, None));
        }
        let Some(job) = &self.job else {
            return Ok((None, None));
        };
        // The reset that clears the planner forgets the program's state, so
        // the job has to be one that can be picked up again.
        let index = job
            .record
            .acked_lines
            .saturating_sub(s.planner_blocks + 1)
            .min(job.lines.len().saturating_sub(1));
        let mut machine = Interpreter::new([0.0; 3], [[0.0; 3]; 6], start_system(&job.record));
        for line in &job.lines[..index] {
            if let Err(e) = machine.step(&line.text) {
                return Ok((
                    None,
                    Some(format!(
                        "line {} uses {}, so the job could not be picked up again",
                        line.number, e.0
                    )),
                ));
            }
        }
        if !machine.modal.absolute {
            return Ok((
                None,
                Some("the program is in incremental mode (G91), so the job could not be picked up again".into()),
            ));
        }
        Ok((Some(settings), None))
    }

    fn start(&mut self, s: &Session, id: &str) -> Result<JobRecord, ControlError> {
        let Some(mut record) = self.store.get(id)? else {
            return Err(ControlError::NotFound);
        };
        self.check_ready()?;
        let lines = self.store.program(id)?;
        if lines.is_empty() {
            return Err(ControlError::Invalid(
                "the program has no lines to send".into(),
            ));
        }
        self.check_program(s, &lines)?;
        let work = self.work_offsets()?.clone();
        record.state = JobState::Running;
        record.total_lines = lines.len();
        record.acked_lines = 0;
        record.last_acked_line = None;
        record.started_at_ms = Some(now_ms());
        record.finished_at_ms = None;
        record.resumed_from_line = None;
        record.paused_at = None;
        record.work_offsets = Some(work);
        record.reason = None;
        record.line_errors.clear();
        self.store.save(&record)?;
        info!(id = %record.id, lines = lines.len(), "job started");
        self.job = Some(ActiveJob {
            record: record.clone(),
            lines,
            next: 0,
            preamble: VecDeque::new(),
            stop_requested: None,
            resume_pending: false,
            lift_pause: None,
            dirty: false,
            last_saved: Instant::now(),
        });
        Ok(record)
    }

    /// Refuses to start or resume unless the machine is ready for a job.
    fn check_ready(&self) -> Result<(), ControlError> {
        if let Some(check) = self.position.check.as_ref().filter(|c| !c.adjusted) {
            return Err(ControlError::Conflict(check.question.clone()));
        }
        if let Some(active) = &self.job {
            return Err(ControlError::Conflict(format!(
                "job {} is already active",
                active.record.id
            )));
        }
        if let Some(code) = self.machine.alarm {
            return Err(ControlError::Conflict(format!(
                "the machine is in alarm {code}: {}",
                grbl::alarm_message(code)
            )));
        }
        if let Some(state) = self.machine.status.as_ref().map(|st| st.state.as_str())
            && state != "Idle"
        {
            return Err(ControlError::Conflict(format!(
                "the machine is {state}, not Idle"
            )));
        }
        if !self.machine.ready {
            return Err(ControlError::Unavailable(
                "the controller has not been identified yet".into(),
            ));
        }
        Ok(())
    }

    /// Checks a program fits the controller and the machine's boundaries.
    fn check_program(&self, s: &Session, lines: &[ProgramLine]) -> Result<(), ControlError> {
        let capacity = self.capacity(s);
        if let Some(long) = lines.iter().find(|l| l.text.len() + 1 > capacity) {
            return Err(ControlError::Invalid(format!(
                "line {} is longer than the controller's buffer",
                long.number
            )));
        }
        let Some(limits) = self.store.bounds()? else {
            return Ok(());
        };
        let Some(position) = self.known_position().0 else {
            return Err(ControlError::Unavailable(
                "boundaries are set but the machine position has not been reported yet; try again"
                    .into(),
            ));
        };
        let work = self.work_offsets()?;
        bounds::check(&limits, lines, position, work).map_err(ControlError::Invalid)
    }

    /// The work coordinate systems, once they have been read from the
    /// controller.
    fn work_offsets(&self) -> Result<&WorkOffsets, ControlError> {
        self.machine.work.as_ref().ok_or_else(|| {
            ControlError::Unavailable(
                "the work coordinate systems have not been read yet; try again".into(),
            )
        })
    }

    /// Continues a job that ended early: interrupted by a lost connection,
    /// a restart of the server or a reset of the controller, or stopped or
    /// failed by an operator or an alarm.
    ///
    /// The saved progress can lag the controller, and a reset throws away
    /// whatever was still in its planner, so the job backs up past the
    /// planner depth. Re-running absolute moves is harmless; incremental
    /// ones are not, so those programs cannot be resumed.
    fn resume_job(
        &mut self,
        s: &Session,
        id: &str,
        options: &ResumeOptions,
    ) -> Result<JobRecord, ControlError> {
        let Some(mut record) = self.store.get(id)? else {
            return Err(ControlError::NotFound);
        };
        self.check_ready()?;
        let lines = self.store.program(id)?;
        self.check_program(s, &lines)?;
        let work = self.work_offsets()?.clone();

        let index = match options.from_line {
            Some(from) => lines.iter().position(|l| l.number >= from).ok_or_else(|| {
                ControlError::Invalid(format!("line {from} is past the end of the program"))
            })?,
            None => record
                .acked_lines
                .saturating_sub(s.planner_blocks + 1)
                .min(lines.len().saturating_sub(1)),
        };

        let (position, _) = self.known_position();
        let mut machine = Interpreter::new(
            position.unwrap_or([0.0; 3]),
            work.zeros,
            start_system(&record),
        );
        for line in &lines[..index] {
            machine.step(&line.text).map_err(|e| {
                ControlError::Invalid(format!(
                    "line {} uses {}, which cannot be restored on resume; start the job again from the top",
                    line.number, e.0
                ))
            })?;
        }
        if !machine.modal.absolute {
            return Err(ControlError::Invalid(format!(
                "the program is in incremental mode (G91) at line {}, so repeating lines is not safe; start the job again from the top",
                lines[index].number
            )));
        }

        // Only the systems the rest of the program cuts in matter.
        if !options.accept_moved_zero {
            let used = systems_cut(&lines[index..], machine.modal.wcs);
            for (system, then) in recorded_zeros(&record) {
                let Some(now) = work.zero(system) else {
                    continue;
                };
                if used.contains(&system) && then.iter().zip(now).any(|(a, b)| (a - b).abs() > 1e-3)
                {
                    return Err(ControlError::Conflict(format!(
                        "the work zero (G{system}) has moved since this job last ran: it was at machine X{:.3} Y{:.3} Z{:.3} and is now at X{:.3} Y{:.3} Z{:.3}, so resuming would cut in a different place; put the zero back, or set accept_moved_zero to resume anyway",
                        then[0], then[1], then[2], now[0], now[1], now[2]
                    )));
                }
            }
        }

        // A pause that freed the machine promised to put the tool back.
        let auto_return = options.retract.is_none()
            && record.state == JobState::Paused
            && record.paused_at.is_some();
        let retract = match (&options.retract, &record.paused_at) {
            (Some(retract), _) => Some(retract.clone()),
            (None, Some(point)) if auto_return => {
                let Some(position) = position else {
                    return Err(ControlError::Unavailable(
                        "the machine position has not been reported yet; try again".into(),
                    ));
                };
                let settings = self.store.settings()?.pause;
                let z = [
                    point.lifted_to.unwrap_or(point.position[2]),
                    position[2],
                    machine.position[2],
                ]
                .into_iter()
                .fold(f64::MIN, f64::max);
                Some(Retract {
                    z,
                    plunge_feed: machine
                        .modal
                        .feed_mm_per_min()
                        .is_none_or(|f| f <= 0.0)
                        .then_some(FALLBACK_PLUNGE_FEED),
                    dwell_s: (!machine.modal.tool_lines().is_empty()).then_some(settings.dwell_s),
                })
            }
            _ => None,
        };
        let preamble = match &retract {
            None => machine.modal.preamble(),
            Some(retract) => {
                let Some(position) = position else {
                    return Err(ControlError::Unavailable(
                        "the machine position has not been reported yet; try again".into(),
                    ));
                };
                self.retract_lines(retract, &machine.modal, machine.position, position[2])?
            }
        };

        record.state = JobState::Running;
        record.acked_lines = index;
        record.last_acked_line = index.checked_sub(1).map(|i| lines[i].number);
        record.resumed_from_line = Some(lines[index].number);
        record.paused_at = None;
        record.work_offsets = Some(WorkOffsets {
            system: start_system(&record),
            zeros: work.zeros,
        });
        record.work_offset = None;
        record.finished_at_ms = None;
        record.reason = None;
        self.store.save(&record)?;
        info!(id = %record.id, from = lines[index].number, retract = retract.is_some(), "job resumed");
        self.job = Some(ActiveJob {
            record: record.clone(),
            lines,
            next: index,
            preamble: preamble.into(),
            stop_requested: None,
            resume_pending: false,
            lift_pause: None,
            dirty: false,
            last_saved: Instant::now(),
        });
        Ok(record)
    }

    /// The machine position and the active work offset from the latest
    /// status report.
    fn known_position(&self) -> (Option<[f64; 3]>, Option<[f64; 3]>) {
        let status = self.machine.status.as_ref();
        (
            three(status.and_then(|st| st.machine_position.as_ref())),
            three(status.and_then(|st| st.work_offset.as_ref())),
        )
    }

    /// The lines that lift the tool, move it above `target`, bring the
    /// spindle back, plunge to `target`, and then restore the program's
    /// modal state. Everything before the plunge is in machine coordinates
    /// and millimetres, so the program's own units cannot misplace it.
    fn retract_lines(
        &self,
        retract: &Retract,
        modal: &Modal,
        target: [f64; 3],
        current_z: f64,
    ) -> Result<Vec<String>, ControlError> {
        const SLACK: f64 = 1e-6;
        let invalid = |message: String| Err(ControlError::Invalid(message));
        if !retract.z.is_finite() {
            return invalid("retract z must be a number".into());
        }
        if retract.z < current_z - SLACK {
            return invalid(format!(
                "retract z {} is below the tool's current Z {current_z:.3}; a retract has to go up",
                retract.z
            ));
        }
        if retract.z < target[2] - SLACK {
            return invalid(format!(
                "retract z {} is below the resume point's Z {:.3}; a retract has to go up",
                retract.z, target[2]
            ));
        }
        if let Some(Bounds {
            z: Some([low, high]),
            ..
        }) = self.store.bounds()?
            && (retract.z < low - SLACK || retract.z > high + SLACK)
        {
            return invalid(format!(
                "retract z {} is outside the Z boundary {low} to {high}",
                retract.z
            ));
        }
        if let Some(dwell) = retract.dwell_s
            && !(0.0..=60.0).contains(&dwell)
        {
            return invalid("dwell_s must be between 0 and 60 seconds".into());
        }
        let feed = match retract.plunge_feed {
            Some(f) if f.is_finite() && f > 0.0 => f,
            Some(_) => return invalid("plunge_feed must be a positive number".into()),
            None => match modal.feed_mm_per_min() {
                Some(f) if f > 0.0 => f,
                _ => {
                    return invalid(
                        "the program has no plain feed rate at the resume point; give plunge_feed"
                            .into(),
                    );
                }
            },
        };

        let mut lines = vec![
            "G21 G90 G94".to_string(),
            format!("G53 G0 Z{:.3}", retract.z),
            format!("G53 G0 X{:.3} Y{:.3}", target[0], target[1]),
        ];
        lines.extend(modal.tool_lines());
        if let Some(dwell) = retract.dwell_s.filter(|d| *d > 0.0) {
            lines.push(format!("G4 P{dwell}"));
        }
        lines.push(format!("G53 G1 Z{:.3} F{feed:.3}", target[2]));
        lines.push(modal.state_line());
        lines.extend(modal.feed_line());
        Ok(lines)
    }

    async fn machine_action(
        &mut self,
        s: &mut Session,
        action: MachineAction,
    ) -> std::io::Result<Result<(), ControlError>> {
        if let Some(job) = &self.job {
            return Ok(Err(ControlError::Conflict(format!(
                "job {} is active; stop it first",
                job.record.id
            ))));
        }
        if !self.machine.ready && action != MachineAction::Reset {
            return Ok(Err(ControlError::Unavailable(
                "the controller has not been identified yet".into(),
            )));
        }
        self.machine.last_error = None;
        match action {
            MachineAction::Unlock => self.send_line(s, "$X", Origin::Internal).await?,
            MachineAction::Home => self.send_line(s, "$H", Origin::Internal).await?,
            MachineAction::Reset => {
                self.jog_target = None;
                self.soft_reset(s).await?
            }
            MachineAction::JogCancel => {
                self.jog_target = None;
                s.writer.write_all(&[realtime::JOG_CANCEL]).await?;
            }
        }
        Ok(Ok(()))
    }

    /// Checks a jog is safe to send and works out where it ends.
    ///
    /// Returns the `$J=` line and, when the position is known, the machine
    /// coordinates the tool will be at once every queued jog has run.
    fn plan_jog(&self, request: &JogRequest) -> Result<(String, Option<[f64; 3]>), ControlError> {
        let invalid = |m: &str| Err(ControlError::Invalid(m.into()));
        if let Some(job) = &self.job {
            return Err(ControlError::Conflict(format!(
                "job {} is active; stop it first",
                job.record.id
            )));
        }
        if let Some(code) = self.machine.alarm {
            return Err(ControlError::Conflict(format!(
                "the machine is in alarm {code}: {}",
                grbl::alarm_message(code)
            )));
        }
        if !self.machine.ready {
            return Err(ControlError::Unavailable(
                "the controller has not been identified yet".into(),
            ));
        }
        match self.machine.status.as_ref().map(|st| st.state.as_str()) {
            Some("Idle" | "Jog") => {}
            Some(state) => {
                return Err(ControlError::Conflict(format!(
                    "the machine is {state}, not Idle"
                )));
            }
            None => {
                return Err(ControlError::Unavailable(
                    "the machine state has not been reported yet".into(),
                ));
            }
        }
        let axes = [request.x, request.y, request.z];
        if axes.iter().all(Option::is_none) {
            return invalid("a jog needs at least one of x, y or z");
        }
        if axes.iter().flatten().any(|v| !v.is_finite()) {
            return invalid("x, y and z must be numbers");
        }
        let feed = match request.feed {
            Some(feed) if feed.is_finite() && feed > 0.0 => feed,
            Some(_) => return invalid("feed must be a positive number"),
            None => self
                .store
                .settings()?
                .jog
                .feed_for(axes.map(|a| a.is_some())),
        };
        if request.mode == JogMode::Relative && request.space.is_some() {
            return invalid("space only applies to absolute jogs");
        }

        let (position, offset) = self.known_position();
        // Jogs already queued have not moved the tool yet, so a second one
        // is measured from where the first will end.
        let moving = self
            .machine
            .status
            .as_ref()
            .is_some_and(|st| st.state != "Idle");
        let base = match self.jog_target {
            Some((target, at)) if moving || at.elapsed() < self.config.poll_interval * 3 => {
                Some(target)
            }
            _ => position,
        };
        let space = request.space.unwrap_or_default();
        let mut line = String::from("$J=G21 ");
        line.push_str(match (request.mode, space) {
            (JogMode::Relative, _) => "G91",
            (JogMode::Absolute, JogSpace::Work) => "G90",
            (JogMode::Absolute, JogSpace::Machine) => "G90 G53",
        });
        let mut wanted = [None; 3];
        for (i, name) in ["X", "Y", "Z"].iter().enumerate() {
            let Some(v) = axes[i] else { continue };
            line.push_str(&format!(" {name}{v:.3}"));
            wanted[i] = match (request.mode, space) {
                (JogMode::Relative, _) => base.map(|b| b[i] + v),
                (JogMode::Absolute, JogSpace::Machine) => Some(v),
                (JogMode::Absolute, JogSpace::Work) => offset.map(|o| o[i] + v),
            };
        }
        line.push_str(&format!(" F{feed:.3}"));

        if let Some(limits) = self.store.bounds()? {
            if axes
                .iter()
                .zip(&wanted)
                .any(|(a, w)| a.is_some() && w.is_none())
            {
                return Err(ControlError::Unavailable(
                    "boundaries are set but the machine position or work offset has not been reported yet; try again".into(),
                ));
            }
            limits.check_point(wanted).map_err(ControlError::Invalid)?;
        }
        let end = base.map(|b| {
            let mut end = b;
            for i in 0..3 {
                if let Some(v) = wanted[i] {
                    end[i] = v;
                }
            }
            end
        });
        Ok((line, end))
    }

    /// Sets the G54 work zero with `G10`. It needs the machine at rest,
    /// because `L20` takes the position the tool is at now.
    async fn work(
        &mut self,
        s: &mut Session,
        change: WorkChange,
        values: AxisValues,
        reply: Reply<()>,
    ) -> std::io::Result<()> {
        let line = match self.plan_work(s, change, &values) {
            Ok(line) => line,
            Err(e) => {
                let _ = reply.send(Err(e));
                return Ok(());
            }
        };
        self.machine.last_error = None;
        let bytes = line.len() + 1;
        if s.inflight.is_empty() {
            s.last_ack = Instant::now();
        }
        s.writer.write_all(format!("{line}\n").as_bytes()).await?;
        s.inflight.push_back(InFlight {
            origin: Origin::Work,
            bytes,
            reply: Some(reply),
        });
        s.inflight_bytes += bytes;
        Ok(())
    }

    fn plan_work(
        &self,
        s: &Session,
        change: WorkChange,
        values: &AxisValues,
    ) -> Result<String, ControlError> {
        self.check_ready()?;
        if s.awaiting_reset.is_some() {
            return Err(ControlError::Unavailable(
                "the controller is resetting".into(),
            ));
        }
        if self.machine.status.is_none() {
            return Err(ControlError::Unavailable(
                "the machine state has not been reported yet".into(),
            ));
        }
        let work = self.work_offsets()?;
        let axes = [values.x, values.y, values.z];
        if axes.iter().flatten().any(|v| !v.is_finite()) {
            return Err(ControlError::Invalid("x, y and z must be numbers".into()));
        }
        let system = match (&values.system, change) {
            (None, WorkChange::Select) => {
                return Err(ControlError::Invalid(
                    "give the system to make active, G54 to G59".into(),
                ));
            }
            (Some(name), _) => work::parse_system(name).map_err(ControlError::Invalid)?,
            (None, _) => work.system,
        };
        let line = match change {
            WorkChange::Select => {
                if axes.iter().any(Option::is_some) {
                    return Err(ControlError::Invalid(
                        "making a system active takes no x, y or z".into(),
                    ));
                }
                format!("G21 G90 G{system}")
            }
            WorkChange::Zero | WorkChange::Offset => {
                if axes.iter().all(Option::is_none) {
                    return Err(ControlError::Invalid(
                        "give at least one of x, y or z".into(),
                    ));
                }
                // P1 is G54, P6 is G59.
                let p = work::index(system).unwrap_or_default() + 1;
                let mut line = format!(
                    "G21 G90 G10 L{} P{p}",
                    if change == WorkChange::Zero { 20 } else { 2 }
                );
                for (name, v) in ["X", "Y", "Z"].iter().zip(axes) {
                    if let Some(v) = v {
                        line.push_str(&format!(" {name}{v:.3}"));
                    }
                }
                line
            }
        };
        if s.inflight_bytes + line.len() + 1 > self.capacity(s) {
            return Err(ControlError::Conflict(
                "the controller is still busy with earlier commands; try again".into(),
            ));
        }
        Ok(line)
    }

    async fn jog(
        &mut self,
        s: &mut Session,
        request: JogRequest,
        reply: Reply<()>,
    ) -> std::io::Result<()> {
        if s.awaiting_reset.is_some() {
            let _ = reply.send(Err(ControlError::Unavailable(
                "the controller is resetting".into(),
            )));
            return Ok(());
        }
        let (line, end) = match self.plan_jog(&request) {
            Ok(planned) => planned,
            Err(e) => {
                let _ = reply.send(Err(e));
                return Ok(());
            }
        };
        let bytes = line.len() + 1;
        if s.inflight_bytes + bytes > self.capacity(s) {
            let _ = reply.send(Err(ControlError::Conflict(
                "the controller is still busy with earlier commands; try again".into(),
            )));
            return Ok(());
        }
        self.machine.last_error = None;
        if s.inflight.is_empty() {
            s.last_ack = Instant::now();
        }
        s.writer.write_all(format!("{line}\n").as_bytes()).await?;
        s.inflight.push_back(InFlight {
            origin: Origin::Jog,
            bytes,
            reply: Some(reply),
        });
        s.inflight_bytes += bytes;
        self.jog_target = end.map(|e| (e, Instant::now()));
        Ok(())
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
        self.job
            .as_ref()
            .map(|j| j.record.clone())
            .expect("active job")
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
        if s.inflight.is_empty() {
            s.last_ack = Instant::now();
        }
        while let Some(text) = job.preamble.front() {
            let bytes = text.len() + 1;
            if s.inflight_bytes + bytes > capacity {
                break;
            }
            out.extend_from_slice(text.as_bytes());
            out.push(b'\n');
            s.inflight.push_back(InFlight {
                origin: Origin::Preamble,
                bytes,
                reply: None,
            });
            s.inflight_bytes += bytes;
            job.preamble.pop_front();
        }
        // Job lines wait until the preamble has gone out.
        while job.preamble.is_empty()
            && let Some(line) = job.lines.get(job.next)
        {
            let bytes = line.text.len() + 1;
            if s.inflight_bytes + bytes > capacity {
                break;
            }
            out.extend_from_slice(line.text.as_bytes());
            out.push(b'\n');
            s.inflight.push_back(InFlight {
                origin: Origin::Job(job.next),
                bytes,
                reply: None,
            });
            s.inflight_bytes += bytes;
            job.next += 1;
        }
        if !out.is_empty() {
            s.writer.write_all(&out).await?;
        }
        Ok(())
    }

    async fn send_line(
        &mut self,
        s: &mut Session,
        text: &str,
        origin: Origin,
    ) -> std::io::Result<()> {
        let bytes = text.len() + 1;
        if s.inflight.is_empty() {
            s.last_ack = Instant::now();
        }
        s.writer.write_all(format!("{text}\n").as_bytes()).await?;
        s.inflight.push_back(InFlight {
            origin,
            bytes,
            reply: None,
        });
        s.inflight_bytes += bytes;
        Ok(())
    }

    async fn soft_reset(&mut self, s: &mut Session) -> std::io::Result<()> {
        s.writer.write_all(&[realtime::SOFT_RESET]).await?;
        s.awaiting_reset = Some(Instant::now());
        Ok(())
    }

    /// Drops everything in flight. A position change that was waiting for
    /// its zero sync is abandoned with it, and reconciliation is due again:
    /// the record still holds the frame the jobs were stored in, so making
    /// the change again from it is safe, while leaving it pending would stop
    /// the position from ever being recorded.
    fn forget_inflight(&mut self, s: &mut Session) {
        clear_inflight(s);
        if self.pending.take().is_some() {
            s.reconciled = false;
        }
    }

    /// The controller has reset after a stop: nothing sent before survives.
    async fn after_reset(&mut self, s: &mut Session) -> std::io::Result<()> {
        s.awaiting_reset = None;
        self.forget_inflight(s);
        self.jog_target = None;
        s.info.next_at = None;
        let Some(job) = &mut self.job else {
            return Ok(());
        };
        if job.stop_requested.is_none() {
            return Ok(());
        }
        if job.lift_pause.is_some() {
            // The reset is done, so the machine no longer needs bringing
            // to rest; what is left is the lift.
            job.stop_requested = None;
            self.begin_lift(s).await
        } else {
            self.finish_job(JobState::Stopped, Some("stopped on request".into()));
            Ok(())
        }
    }

    /// Where a pause should lift the tool to, or why it cannot.
    fn lift_target(&self, lift: &LiftPause) -> Result<Option<f64>, String> {
        let Some(position) = lift.position else {
            return Err("the tool's position was not known".into());
        };
        let mut z = lift
            .settings
            .z
            .unwrap_or(position[2] + lift.settings.height);
        if let Ok(Some(Bounds {
            z: Some([_, high]), ..
        })) = self.store.bounds()
        {
            z = z.min(high);
        }
        Ok((z > position[2] + 1e-3).then_some(z))
    }

    /// Lifts the tool after the reset that followed a pause, or ends the
    /// pause at once if there is nothing to lift.
    async fn begin_lift(&mut self, s: &mut Session) -> std::io::Result<()> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        let Some(lift) = &job.lift_pause else {
            return Ok(());
        };
        match self.lift_target(lift) {
            Ok(Some(z)) => {
                // Write down where the lift is headed before sending it. If
                // the server stops mid-lift, the job is then still a paused
                // job with a height to return the tool to, not an
                // interrupted one with no idea where the tool is.
                if let (Some(position), Some(job)) = (lift.position, self.job.as_mut()) {
                    job.record.lift_pending = Some(PausePoint {
                        position,
                        lifted_to: Some(z),
                    });
                    self.save_progress(true);
                }
                // A jog is refused by the controller if it would leave the
                // machine's travel, where a plain move raises an alarm.
                let line = format!("$J=G21 G90 G53 Z{z:.3} F{LIFT_FEED:.3}");
                let bytes = line.len() + 1;
                s.writer.write_all(format!("{line}\n").as_bytes()).await?;
                s.last_ack = Instant::now();
                s.inflight.push_back(InFlight {
                    origin: Origin::Lift,
                    bytes,
                    reply: None,
                });
                s.inflight_bytes += bytes;
                if let Some(lift) = self.job.as_mut().and_then(|j| j.lift_pause.as_mut()) {
                    lift.lifting = true;
                    lift.lifted_to = Some(z);
                }
            }
            Ok(None) => self.release_paused(Some("the tool was already as high as it can go")),
            Err(why) => self.release_paused(Some(&why)),
        }
        Ok(())
    }

    /// Ends a pause that stopped the machine: the job keeps its place on
    /// disk and the machine is free to move.
    fn release_paused(&mut self, note: Option<&str>) {
        let Some(job) = &mut self.job else {
            return;
        };
        let Some(lift) = job.lift_pause.take() else {
            return;
        };
        let position = lift
            .position
            .or_else(|| {
                three(
                    self.machine
                        .status
                        .as_ref()
                        .and_then(|st| st.machine_position.as_ref()),
                )
            })
            .unwrap_or_default();
        job.record.paused_at = Some(PausePoint {
            position,
            lifted_to: lift.lifted_to,
        });
        let reason = match (lift.lifted_to, note.or(lift.note.as_deref())) {
            (Some(_), _) => "paused on request; the tool was lifted".to_string(),
            (None, Some(why)) => format!("paused on request; the tool was not lifted: {why}"),
            (None, None) => "paused on request".to_string(),
        };
        self.finish_job(JobState::Paused, Some(reason));
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
        // A lift that is not pending any more has either been released into
        // `paused_at` or the job has ended.
        job.record.lift_pending = None;
        if state != JobState::Paused {
            // Only a released pause keeps a position: it is what a resume
            // brings the tool back from.
            job.record.paused_at = None;
            job.record.finished_at_ms = Some(now_ms());
        }
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

    /// Records where the tool is, once a connection has been reconciled.
    /// Writes are throttled while the machine moves, and not made while a
    /// check is waiting, so the record keeps the position the operator is
    /// being asked about.
    fn record_position(&mut self, status: &Status) {
        if self.pending.is_some() || self.position.check.as_ref().is_some_and(|c| !c.adjusted) {
            return;
        }
        let (Some(machine), Some(zeros)) = (
            machine_position(status),
            self.machine.work.as_ref().map(|w| w.zeros),
        ) else {
            return;
        };
        if self.position.record.is_some_and(|r| {
            !position::differs(r.machine, machine) && !position::zeros_differ(r.zeros, zeros)
        }) {
            return;
        }
        let at_rest = status.state == "Idle" || status.hold_complete();
        if !at_rest && self.last_position_save.elapsed() < SAVE_INTERVAL {
            return;
        }
        self.position.record = Some(Record { machine, zeros });
        self.last_position_save = Instant::now();
        self.save_position();
    }

    /// Compares the position the controller reports with the recorded one,
    /// once per connection, when it is idle and its work zeros are known.
    ///
    /// The sender is the only thing that moves the machine, so a difference
    /// means the controller lost its position (a reset, say) and not that the
    /// tool moved. By default every work zero moves by the same amount, to
    /// keep the tool where the record puts it, and the operator is asked
    /// whether the machine moved. With `position.trust_record` off, jobs wait
    /// for the answer instead.
    async fn reconcile(&mut self, s: &mut Session, status: &Status) -> std::io::Result<()> {
        let (Some(reported), Some(zeros)) = (
            machine_position(status),
            self.machine.work.as_ref().map(|w| w.zeros),
        ) else {
            return Ok(());
        };
        s.reconciled = true;
        let agreed = |record: Record| Change {
            zeros: None,
            shift: [0.0; 3],
            record: Record {
                machine: reported,
                zeros: record.zeros,
            },
            check: None,
        };
        let Some(record) = self.position.record else {
            let change = Change {
                zeros: None,
                shift: [0.0; 3],
                record: Record {
                    machine: reported,
                    zeros,
                },
                check: None,
            };
            return self.apply(s, change).await;
        };
        if !position::differs(record.machine, reported) {
            let change = agreed(Record { zeros, ..record });
            return self.apply(s, change).await;
        }

        let trust = match self.store.settings() {
            Ok(settings) => settings.position.trust_record,
            Err(e) => {
                warn!(error = %e, "cannot read settings; trusting the recorded position");
                true
            }
        };
        let d = position::delta(record.machine, reported);
        warn!(
            recorded = %position::fmt(record.machine),
            reported = %position::fmt(reported),
            trust,
            "the controller reports a different position than the one recorded"
        );
        let question = format!(
            "The controller reports the tool at {}, but it was last recorded at {}. {} Has the machine moved since? Answer with POST /machine/position-check, {{\"moved\": true}} or {{\"moved\": false}}.",
            position::fmt(reported),
            position::fmt(record.machine),
            if trust {
                "The work zeros have been moved to keep the tool where it was recorded."
            } else {
                "Jobs wait until you answer."
            }
        );
        let check = |zeros, adjusted| Check {
            recorded: record.machine,
            reported,
            previous_zeros: record.zeros,
            zeros,
            adjusted,
            question: question.clone(),
        };
        if trust {
            let target = position::shift_zeros(record.zeros, d);
            let change = Change {
                zeros: Some((record.zeros, target)),
                shift: d,
                record: Record {
                    machine: reported,
                    zeros: target,
                },
                check: Some(check(target, true)),
            };
            self.apply(s, change).await
        } else {
            self.position.check = Some(check(zeros, false));
            self.publish();
            self.save_position();
            Ok(())
        }
    }

    /// Acts on the operator's answer to a position check.
    async fn answer_position_check(
        &mut self,
        s: &mut Session,
        moved: bool,
    ) -> std::io::Result<Result<(), ControlError>> {
        let Some(check) = self.position.check.clone() else {
            return Ok(Err(ControlError::Conflict(
                "there is no position check waiting for an answer".into(),
            )));
        };
        if let Some(job) = &self.job {
            return Ok(Err(ControlError::Conflict(format!(
                "job {} is active; stop it first",
                job.record.id
            ))));
        }
        if self.pending.is_some() {
            return Ok(Err(ControlError::Conflict(
                "the work zeros are still being moved; try again in a moment".into(),
            )));
        }
        let Some(status) = self.machine.status.clone() else {
            return Ok(Err(ControlError::Unavailable(
                "the machine position has not been reported yet".into(),
            )));
        };
        if status.state != "Idle" {
            return Ok(Err(ControlError::Conflict(format!(
                "the machine is {}, not Idle",
                status.state
            ))));
        }
        let (Some(now), Some(zeros_now)) = (
            machine_position(&status),
            self.machine.work.as_ref().map(|w| w.zeros),
        ) else {
            return Ok(Err(ControlError::Unavailable(
                "the machine position or work zeros have not been read yet".into(),
            )));
        };
        let d = position::delta(check.recorded, check.reported);
        let change = match (moved, check.adjusted) {
            // Moved after the zeros were moved: put them back and take the
            // report as it stands. The frame does not change.
            (true, true) => Change {
                zeros: Some((check.zeros, check.previous_zeros)),
                shift: [0.0; 3],
                record: Record {
                    machine: now,
                    zeros: check.previous_zeros,
                },
                check: None,
            },
            // Moved, with the zeros untouched: the report is the truth from
            // here on, so the frame becomes the controller's.
            (true, false) => Change {
                zeros: None,
                shift: d,
                record: Record {
                    machine: now,
                    zeros: zeros_now,
                },
                check: None,
            },
            // Not moved, and the zeros were already moved: that stands.
            (false, true) => Change {
                zeros: None,
                shift: [0.0; 3],
                record: Record {
                    machine: now,
                    zeros: zeros_now,
                },
                check: None,
            },
            // Not moved: the record was right, so move the zeros to keep the
            // tool where it is recorded.
            (false, false) => {
                let to = position::shift_zeros(check.previous_zeros, d);
                Change {
                    zeros: Some((check.previous_zeros, to)),
                    shift: d,
                    record: Record {
                        machine: now,
                        zeros: to,
                    },
                    check: None,
                }
            }
        };
        info!(moved, "position check answered");
        self.apply(s, change).await?;
        Ok(Ok(()))
    }

    /// Makes a change, sending the work zeros first when they move. The
    /// record only changes once the controller has accepted every one.
    async fn apply(&mut self, s: &mut Session, change: Change) -> std::io::Result<()> {
        match change.zeros {
            Some((_, to)) => {
                let outstanding = to.len();
                for (i, zero) in to.iter().enumerate() {
                    // Millimetres and absolute mode go on each line, as they do
                    // for the operator's work-zero changes.
                    let line = format!(
                        "G21 G90 G10 L2 P{} X{:.3} Y{:.3} Z{:.3}",
                        i + 1,
                        zero[0],
                        zero[1],
                        zero[2]
                    );
                    self.send_line(s, &line, Origin::Sync).await?;
                }
                self.pending = Some(Pending {
                    change,
                    outstanding,
                });
            }
            None => self.commit(change),
        }
        Ok(())
    }

    fn commit(&mut self, change: Change) {
        let moves = change.zeros.is_some() || change.shift != [0.0; 3];
        if moves && let Err(e) = self.store.rebase_jobs(change.zeros, change.shift) {
            warn!(error = %e, "cannot move the stored jobs to the new frame");
        }
        if let Some((_, to)) = change.zeros
            && let Some(work) = self.machine.work.as_mut()
        {
            work.zeros = to;
            let active = to[work::index(work.system).unwrap_or_default()];
            if let Some(st) = self.machine.status.as_mut() {
                st.work_offset = Some(active.to_vec());
                st.work_position = None;
            }
        }
        self.position = Position {
            record: Some(change.record),
            check: change.check,
        };
        self.last_position_save = Instant::now();
        self.save_position();
        self.publish();
    }

    fn save_position(&self) {
        if let Err(e) = self.store.set_position(&self.position) {
            warn!(error = %e, "cannot save the tool position");
        }
    }

    fn publish(&mut self) {
        self.machine.active_job = self.job.as_ref().map(|j| j.record.clone());
        self.machine.position_check = self.position.check.clone();
        self.snapshot_tx.send_replace(self.machine.clone());
    }
}

pub fn state_name(state: JobState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Whether a pause is lifting the tool, for the job as it stands.
fn lift_planned(job: &Option<ActiveJob>) -> bool {
    job.as_ref().is_some_and(|j| j.lift_pause.is_some())
}

/// The work coordinate system a job started in. Records from before
/// several systems were supported only ever used G54.
fn start_system(record: &JobRecord) -> u32 {
    record.work_offsets.as_ref().map_or(54, |w| w.system)
}

/// The zeros a job last ran with, in machine coordinates, by system.
fn recorded_zeros(record: &JobRecord) -> Vec<(u32, [f64; 3])> {
    match (&record.work_offsets, record.work_offset) {
        (Some(work), _) => work::SYSTEMS
            .iter()
            .filter_map(|&s| work.zero(s).map(|z| (s, z)))
            .collect(),
        (None, Some(zero)) => vec![(54, zero)],
        (None, None) => Vec::new(),
    }
}

/// The systems a run of lines cuts in: the one active when it starts, and
/// each one a line selects.
fn systems_cut(lines: &[ProgramLine], active: u32) -> BTreeSet<u32> {
    let mut used: BTreeSet<u32> = lines
        .iter()
        .flat_map(|l| gcode::systems_selected(&l.text))
        .collect();
    used.insert(active);
    used
}

/// The machine position of a status report, which may give the work
/// position instead.
fn machine_position(status: &Status) -> Option<[f64; 3]> {
    three(status.machine_position.as_ref()).or_else(|| {
        let work = three(status.work_position.as_ref())?;
        let offset = three(status.work_offset.as_ref())?;
        Some([
            work[0] + offset[0],
            work[1] + offset[1],
            work[2] + offset[2],
        ])
    })
}

fn three(v: Option<&Vec<f64>>) -> Option<[f64; 3]> {
    v.filter(|v| v.len() >= 3).map(|v| [v[0], v[1], v[2]])
}
