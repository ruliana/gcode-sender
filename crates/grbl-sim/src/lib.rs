//! A simulated GRBL-family controller behind a pseudo-terminal.
//!
//! The simulator opens a PTY pair and exposes the slave side through a stable
//! symlink, so the sender under test opens it exactly like a USB serial port.
//! It models the parts of the controller that matter to a sender: the serial
//! RX buffer used for character counting, the planner queue, realtime
//! commands (`?`, `!`, `~`, soft reset), alarms, and error responses.
//!
//! Tests drive the sender over HTTP and use [`Simulator`] to inspect what the
//! controller received and to inject faults.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};

/// Which firmware family to imitate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firmware {
    /// Classic Grbl 1.1, as on the Arduino-based LongBoard.
    Grbl,
    /// grblHAL, as on the SuperLongBoard (SLB).
    GrblHal,
}

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub firmware: Firmware,
    /// Serial RX buffer size reported in `$I` and enforced for overflow checks.
    pub rx_buffer: usize,
    /// Planner queue depth reported in `$I`.
    pub planner_blocks: usize,
    /// How long each motion block takes to execute.
    pub block_duration: Duration,
    /// How long a feed hold takes to decelerate before reaching `Hold:0`.
    pub hold_decel: Duration,
    /// Print the startup banner as soon as the simulator starts.
    pub banner_on_start: bool,
}

impl SimConfig {
    pub fn grbl() -> Self {
        Self {
            firmware: Firmware::Grbl,
            rx_buffer: 128,
            planner_blocks: 15,
            block_duration: Duration::from_millis(5),
            hold_decel: Duration::from_millis(50),
            banner_on_start: true,
        }
    }

    pub fn grblhal() -> Self {
        Self {
            firmware: Firmware::GrblHal,
            rx_buffer: 1024,
            planner_blocks: 35,
            ..Self::grbl()
        }
    }

    pub fn block_duration(mut self, d: Duration) -> Self {
        self.block_duration = d;
        self
    }
}

impl Default for SimConfig {
    fn default() -> Self {
        Self::grbl()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    None,
    Decelerating(Instant),
    Complete,
}

#[derive(Clone, Debug)]
struct Block {
    target: Option<[f64; 3]>,
    remaining: Duration,
}

/// Everything the simulator knows, shared between its thread and the test.
#[derive(Debug)]
struct State {
    config: SimConfig,
    rx: VecDeque<u8>,
    planner: VecDeque<Block>,
    current: Option<Block>,
    hold: Hold,
    alarm: Option<u32>,
    position: [f64; 3],
    absolute: bool,
    out: Vec<u8>,
    // Observations for tests.
    received: Vec<String>,
    realtime: Vec<u8>,
    overflowed: bool,
    max_rx: usize,
    resets: usize,
    // Fault injection.
    error_rules: Vec<(String, u32)>,
}

impl State {
    fn new(config: SimConfig) -> Self {
        Self {
            config,
            rx: VecDeque::new(),
            planner: VecDeque::new(),
            current: None,
            hold: Hold::None,
            alarm: None,
            position: [0.0; 3],
            absolute: true,
            out: Vec::new(),
            received: Vec::new(),
            realtime: Vec::new(),
            overflowed: false,
            max_rx: 0,
            resets: 0,
            error_rules: Vec::new(),
        }
    }

    fn reply(&mut self, s: &str) {
        self.out.extend_from_slice(s.as_bytes());
        self.out.extend_from_slice(b"\r\n");
    }

    fn banner(&mut self) {
        match self.config.firmware {
            Firmware::Grbl => self.reply("Grbl 1.1h ['$' for help]"),
            Firmware::GrblHal => self.reply("GrblHAL 1.1f ['$' or '$HELP' for help]"),
        }
    }

    fn moving(&self) -> bool {
        self.current.is_some() || !self.planner.is_empty()
    }

    fn state_name(&self) -> &'static str {
        if self.alarm.is_some() {
            return "Alarm";
        }
        match self.hold {
            Hold::Decelerating(_) => "Hold:1",
            Hold::Complete => "Hold:0",
            Hold::None if self.moving() => "Run",
            Hold::None => "Idle",
        }
    }

    fn status_report(&mut self) {
        let [x, y, z] = self.position;
        let feed = if self.state_name() == "Run" { 1000 } else { 0 };
        let report = format!(
            "<{}|MPos:{x:.3},{y:.3},{z:.3}|FS:{feed},0>",
            self.state_name()
        );
        self.reply(&report);
    }

    fn feed_byte(&mut self, b: u8) {
        match b {
            b'?' | 0x80 => {
                self.realtime.push(b);
                self.status_report();
            }
            b'!' | 0x81 => {
                self.realtime.push(b);
                if self.alarm.is_none() && self.hold == Hold::None {
                    self.hold = if self.moving() {
                        Hold::Decelerating(Instant::now())
                    } else {
                        Hold::Complete
                    };
                }
            }
            b'~' | 0x82 => {
                self.realtime.push(b);
                if self.hold == Hold::Complete {
                    self.hold = Hold::None;
                }
            }
            0x18 => {
                self.realtime.push(b);
                self.soft_reset();
            }
            0x83..=0xff => self.realtime.push(b),
            _ => {
                self.rx.push_back(b);
                self.max_rx = self.max_rx.max(self.rx.len());
                if self.rx.len() > self.config.rx_buffer {
                    self.overflowed = true;
                }
            }
        }
    }

    fn soft_reset(&mut self) {
        // Resetting mid-motion loses position, so real firmware raises
        // ALARM:3. Resetting from a completed hold is safe.
        let aborted_motion = self.moving() && self.hold != Hold::Complete;
        self.rx.clear();
        self.planner.clear();
        self.current = None;
        self.hold = Hold::None;
        self.resets += 1;
        if aborted_motion {
            self.reply("ALARM:3");
            self.alarm = Some(3);
        }
        self.reply("");
        self.banner();
        if self.alarm.is_some() {
            self.reply("[MSG:'$H'|'$X' to unlock]");
        }
    }

    /// Takes complete lines out of the RX buffer while the planner has room.
    fn process_lines(&mut self) {
        while self.planner.len() < self.config.planner_blocks {
            let Some(end) = self.rx.iter().position(|&b| b == b'\n') else {
                return;
            };
            let raw: Vec<u8> = self.rx.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw).trim().to_string();
            self.received.push(line.clone());
            self.execute_line(&line);
        }
    }

    fn execute_line(&mut self, line: &str) {
        if line.is_empty() {
            self.reply("ok");
            return;
        }
        if let Some(code) = self
            .error_rules
            .iter()
            .find(|(pat, _)| line.contains(pat.as_str()))
            .map(|(_, code)| *code)
        {
            self.reply(&format!("error:{code}"));
            return;
        }
        if let Some(cmd) = line.strip_prefix('$') {
            self.system_command(cmd);
            return;
        }
        if self.alarm.is_some() {
            self.reply("error:9");
            return;
        }
        match self.parse_motion(line) {
            Ok(block) => {
                self.planner.push_back(block);
                self.reply("ok");
            }
            Err(code) => self.reply(&format!("error:{code}")),
        }
    }

    fn system_command(&mut self, cmd: &str) {
        let idle = !self.moving() && self.hold == Hold::None;
        match cmd {
            "X" => {
                self.alarm = None;
                self.reply("[MSG:Caution: Unlocked]");
                self.reply("ok");
            }
            "H" => {
                self.alarm = None;
                self.position = [0.0; 3];
                self.reply("ok");
            }
            _ if !idle => self.reply("error:8"),
            "I" => {
                match self.config.firmware {
                    Firmware::Grbl => {
                        self.reply("[VER:1.1h.20190825:]");
                        let opt = format!(
                            "[OPT:V,{},{}]",
                            self.config.planner_blocks, self.config.rx_buffer
                        );
                        self.reply(&opt);
                    }
                    Firmware::GrblHal => {
                        self.reply("[VER:1.1f.20240512:]");
                        let opt = format!(
                            "[OPT:VNMSL,{},{},3,0]",
                            self.config.planner_blocks, self.config.rx_buffer
                        );
                        self.reply(&opt);
                        self.reply("[NEWOPT:ENUMS,RT+,SED,RET]");
                        self.reply("[FIRMWARE:grblHAL]");
                        self.reply("[BOARD:SuperLongBoard]");
                    }
                }
                self.reply("ok");
            }
            "$" => {
                self.reply("$0=10");
                self.reply("$1=25");
                self.reply("$10=1");
                self.reply("ok");
            }
            "G" => {
                self.reply("[GC:G0 G54 G17 G21 G90 G94 M5 M9 T0 F0 S0]");
                self.reply("ok");
            }
            "#" => {
                self.reply("[G54:0.000,0.000,0.000]");
                self.reply("ok");
            }
            _ => self.reply("error:3"),
        }
    }

    /// A deliberately small G-code model: only the words that move the
    /// simulated position are interpreted. Anything with an X, Y, or Z word
    /// is a motion block that takes `block_duration` to run.
    fn parse_motion(&mut self, line: &str) -> Result<Block, u32> {
        let upper = line.to_ascii_uppercase();
        let mut chars = upper.chars().filter(|c| !c.is_whitespace()).peekable();
        let mut axes: [Option<f64>; 3] = [None; 3];
        let mut absolute = self.absolute;
        while let Some(letter) = chars.next() {
            if !letter.is_ascii_alphabetic() {
                return Err(2);
            }
            let mut number = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' {
                    number.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            let value: f64 = number.parse().map_err(|_| 2u32)?;
            match letter {
                'G' if value == 90.0 => absolute = true,
                'G' if value == 91.0 => absolute = false,
                'X' => axes[0] = Some(value),
                'Y' => axes[1] = Some(value),
                'Z' => axes[2] = Some(value),
                _ => {}
            }
        }
        self.absolute = absolute;
        if axes.iter().all(Option::is_none) {
            return Ok(Block {
                target: None,
                remaining: Duration::ZERO,
            });
        }
        // Targets are resolved against the end of the queued motion.
        let base = self
            .planner
            .iter()
            .rev()
            .chain(self.current.iter())
            .find_map(|b| b.target)
            .unwrap_or(self.position);
        let mut target = base;
        for (i, axis) in axes.iter().enumerate() {
            if let Some(v) = axis {
                target[i] = if absolute { *v } else { base[i] + v };
            }
        }
        Ok(Block {
            target: Some(target),
            remaining: self.config.block_duration,
        })
    }

    fn advance(&mut self, dt: Duration) {
        if let Hold::Decelerating(since) = self.hold
            && since.elapsed() >= self.config.hold_decel
        {
            self.hold = Hold::Complete;
        }
        if self.hold != Hold::None || self.alarm.is_some() {
            return;
        }
        let mut budget = dt;
        loop {
            if self.current.is_none() {
                self.current = self.planner.pop_front();
            }
            let Some(block) = self.current.as_mut() else {
                return;
            };
            if block.remaining > budget {
                block.remaining -= budget;
                return;
            }
            budget -= block.remaining;
            if let Some(target) = block.target {
                self.position = target;
            }
            self.current = None;
        }
    }
}

/// A running simulated controller. Dropping it stops the simulator and
/// closes the serial port.
pub struct Simulator {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    link: PathBuf,
    _slave: OwnedFd,
}

impl Simulator {
    /// Starts a simulator whose serial port appears at `link`.
    pub fn start(config: SimConfig, link: impl AsRef<Path>) -> std::io::Result<Self> {
        let link = link.as_ref().to_path_buf();
        let pty = nix::pty::openpty(None, None)?;
        // Keep the slave side open ourselves so the master never sees EIO
        // while the sender is between connections.
        let mut attrs = tcgetattr(pty.slave.as_fd())?;
        cfmakeraw(&mut attrs);
        tcsetattr(pty.slave.as_fd(), SetArg::TCSANOW, &attrs)?;
        let slave_path = nix::unistd::ttyname(pty.slave.as_fd())?;
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&slave_path, &link)?;

        let flags = OFlag::from_bits_truncate(fcntl(pty.master.as_fd(), FcntlArg::F_GETFL)?);
        fcntl(
            pty.master.as_fd(),
            FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK),
        )?;
        let master = File::from(pty.master);

        let mut state = State::new(config.clone());
        if config.banner_on_start {
            state.reply("");
            state.banner();
        }
        let state = Arc::new(Mutex::new(state));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = state.clone();
            let stop = stop.clone();
            std::thread::spawn(move || run(master, state, stop))
        };
        Ok(Self {
            state,
            stop,
            thread: Some(thread),
            link,
            _slave: pty.slave,
        })
    }

    /// The path the sender should open as its serial port.
    pub fn port(&self) -> &Path {
        &self.link
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Every line the controller has parsed, in order, trimmed.
    pub fn received_lines(&self) -> Vec<String> {
        self.lock().received.clone()
    }

    /// Every realtime byte received, in order.
    pub fn realtime_bytes(&self) -> Vec<u8> {
        self.lock().realtime.clone()
    }

    /// True if the sender ever had more unprocessed bytes in flight than the
    /// RX buffer holds. A correct character-counting sender never does this.
    pub fn overflowed(&self) -> bool {
        self.lock().overflowed
    }

    /// The most bytes ever waiting in the RX buffer at once.
    pub fn max_rx_used(&self) -> usize {
        self.lock().max_rx
    }

    pub fn soft_resets(&self) -> usize {
        self.lock().resets
    }

    /// The controller state as it would appear in a status report.
    pub fn state(&self) -> String {
        self.lock().state_name().to_string()
    }

    pub fn position(&self) -> [f64; 3] {
        self.lock().position
    }

    pub fn alarm(&self) -> Option<u32> {
        self.lock().alarm
    }

    /// Reply `error:<code>` to every line containing `pattern`.
    pub fn error_on(&self, pattern: &str, code: u32) {
        self.lock().error_rules.push((pattern.to_string(), code));
    }

    /// Raise an alarm, as a hard limit switch would.
    pub fn trigger_alarm(&self, code: u32) {
        // Like the real firmware, an alarm aborts motion, throws away
        // whatever is in the RX buffer, and reinitialises with a banner.
        let mut s = self.lock();
        s.rx.clear();
        s.planner.clear();
        s.current = None;
        s.hold = Hold::None;
        s.alarm = Some(code);
        s.reply(&format!("ALARM:{code}"));
        s.reply("");
        s.banner();
        s.reply("[MSG:Reset to continue]");
    }

    pub fn set_block_duration(&self, d: Duration) {
        self.lock().config.block_duration = d;
    }
}

impl Drop for Simulator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.link);
    }
}

fn run(mut master: File, state: Arc<Mutex<State>>, stop: Arc<AtomicBool>) {
    let tick = Duration::from_millis(1);
    let mut buf = [0u8; 4096];
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut incoming = Vec::new();
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => incoming.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let out = {
            let mut s = state.lock().unwrap();
            for b in incoming {
                s.feed_byte(b);
            }
            s.process_lines();
            let now = Instant::now();
            s.advance(now - last);
            last = now;
            // Space freed in the planner lets more lines through.
            s.process_lines();
            std::mem::take(&mut s.out)
        };
        let mut pending = &out[..];
        while !pending.is_empty() && !stop.load(Ordering::Relaxed) {
            match master.write(pending) {
                Ok(n) => pending = &pending[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(tick),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        std::thread::sleep(tick);
    }
}
