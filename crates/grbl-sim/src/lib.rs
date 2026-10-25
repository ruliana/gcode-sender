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
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
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
    jog: bool,
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
    /// G54 to G59, in machine coordinates.
    work_offsets: [[f64; 3]; 6],
    /// Index into `work_offsets` of the active system.
    system: usize,
    /// Real firmware reports the offset on the next status after it changes.
    wco_pending: bool,
    /// Soft limits: jogs past them are refused with error:15.
    travel: Option<([f64; 3], [f64; 3])>,
    reports: usize,
    // Fault injection.
    error_rules: Vec<(String, u32)>,
    stalled: bool,
    drop_acks: usize,
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
            work_offsets: [[0.0; 3]; 6],
            system: 0,
            wco_pending: false,
            travel: None,
            reports: 0,
            error_rules: Vec::new(),
            stalled: false,
            drop_acks: 0,
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

    fn jogging(&self) -> bool {
        self.current
            .iter()
            .chain(self.planner.iter())
            .any(|b| b.jog)
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
            Hold::None if self.jogging() => "Jog",
            Hold::None if self.moving() => "Run",
            Hold::None => "Idle",
        }
    }

    /// Acknowledges a line, unless a test asked for acknowledgements to be
    /// lost on the wire.
    fn ack(&mut self) {
        if self.drop_acks > 0 {
            self.drop_acks -= 1;
        } else {
            self.reply("ok");
        }
    }

    fn status_report(&mut self) {
        let [x, y, z] = self.position;
        let feed = if self.state_name() == "Run" { 1000 } else { 0 };
        let mut report = format!(
            "<{}|MPos:{x:.3},{y:.3},{z:.3}|FS:{feed},0",
            self.state_name()
        );
        // Like real firmware, the work offset rides along only on some reports.
        if self.reports.is_multiple_of(5) || self.wco_pending {
            self.wco_pending = false;
            let [ox, oy, oz] = self.work_offsets[self.system];
            report.push_str(&format!("|WCO:{ox:.3},{oy:.3},{oz:.3}"));
        }
        self.reports += 1;
        report.push('>');
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
            0x85 => {
                self.realtime.push(b);
                if self.jogging() {
                    // Jog cancel stops the motion and flushes the planner.
                    self.planner.clear();
                    self.current = None;
                }
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
            self.ack();
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
        if line.to_ascii_uppercase().contains("G10") {
            match self.set_offset(line) {
                Ok(()) => self.ack(),
                Err(code) => self.reply(&format!("error:{code}")),
            }
            return;
        }
        match self.parse_motion(line) {
            Ok(block) => {
                self.planner.push_back(block);
                self.ack();
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
                self.ack();
            }
            "H" => {
                self.alarm = None;
                self.position = [0.0; 3];
                self.ack();
            }
            _ if cmd.starts_with("J=") => self.jog(&cmd[2..]),
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
                self.ack();
            }
            "$" => {
                self.reply("$0=10");
                self.reply("$1=25");
                self.reply("$10=1");
                self.ack();
            }
            "G" => {
                let system = 54 + self.system;
                self.reply(&format!("[GC:G0 G{system} G17 G21 G90 G94 M5 M9 T0 F0 S0]"));
                self.ack();
            }
            "#" => {
                let lines: Vec<String> = self
                    .work_offsets
                    .iter()
                    .enumerate()
                    .map(|(i, [x, y, z])| format!("[G{}:{x:.3},{y:.3},{z:.3}]", 54 + i))
                    .collect();
                for line in lines {
                    self.reply(&line);
                }
                self.ack();
            }
            _ => self.reply("error:3"),
        }
    }

    /// `G10 L2/L20 P0..P6`: sets a work offset, P1 to P6 being G54 to G59
    /// and P0 the active one. `L2` takes machine coordinates; `L20` takes
    /// the work coordinates of the current position.
    fn set_offset(&mut self, line: &str) -> Result<(), u32> {
        let upper = line.to_ascii_uppercase();
        let mut chars = upper.chars().filter(|c| !c.is_whitespace()).peekable();
        let (mut l, mut p) = (None, None);
        let mut axes: [Option<f64>; 3] = [None; 3];
        let mut absolute = self.absolute;
        while let Some(letter) = chars.next() {
            let mut number = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() || matches!(c, '.' | '-' | '+') {
                    number.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            let value: f64 = number.parse().map_err(|_| 2u32)?;
            match (letter, value as i64) {
                ('G', 10 | 21) => {}
                ('G', 90) => absolute = true,
                ('L', n) => l = Some(n),
                ('P', n) => p = Some(n),
                ('X', _) => axes[0] = Some(value),
                ('Y', _) => axes[1] = Some(value),
                ('Z', _) => axes[2] = Some(value),
                _ => return Err(20),
            }
        }
        let (Some(l), Some(p)) = (l, p) else {
            return Err(28);
        };
        if !matches!(l, 2 | 20) || !(0..=6).contains(&p) {
            return Err(20);
        }
        if self.moving() || self.hold != Hold::None {
            return Err(8);
        }
        self.absolute = absolute;
        let index = if p == 0 {
            self.system
        } else {
            (p - 1) as usize
        };
        for (i, v) in axes.iter().enumerate() {
            if let Some(v) = v {
                self.work_offsets[index][i] = if l == 2 { *v } else { self.position[i] - v };
            }
        }
        self.wco_pending = true;
        Ok(())
    }

    /// `$J=`: a jog. It moves like a G1 but leaves the parser's modal state
    /// alone, and is only accepted when idle or already jogging.
    fn jog(&mut self, words: &str) {
        if self.alarm.is_some() {
            self.reply("error:9");
            return;
        }
        if self.hold != Hold::None || (self.moving() && !self.jogging()) {
            self.reply("error:8");
            return;
        }
        let mut absolute = self.absolute;
        let mut machine_coords = false;
        let mut axes: [Option<f64>; 3] = [None; 3];
        let mut feed = None;
        let upper = words.to_ascii_uppercase();
        let mut chars = upper.chars().filter(|c| !c.is_whitespace()).peekable();
        while let Some(letter) = chars.next() {
            let mut number = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() || matches!(c, '.' | '-' | '+') {
                    number.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            let Ok(value) = number.parse::<f64>() else {
                self.reply("error:2");
                return;
            };
            match (letter, value as i64) {
                ('G', 90) => absolute = true,
                ('G', 91) => absolute = false,
                ('G', 53) => machine_coords = true,
                ('G', 20 | 21) => {}
                ('X', _) => axes[0] = Some(value),
                ('Y', _) => axes[1] = Some(value),
                ('Z', _) => axes[2] = Some(value),
                ('F', _) => feed = Some(value),
                _ => {
                    self.reply("error:20");
                    return;
                }
            }
        }
        if feed.is_none() {
            self.reply("error:22");
            return;
        }
        if axes.iter().all(Option::is_none) {
            self.reply("error:33");
            return;
        }
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
                target[i] = match (absolute, machine_coords) {
                    (false, _) => base[i] + v,
                    (true, true) => *v,
                    (true, false) => v + self.work_offsets[self.system][i],
                };
            }
        }
        if let Some((low, high)) = self.travel
            && (0..3).any(|i| target[i] < low[i] || target[i] > high[i])
        {
            self.reply("error:15");
            return;
        }
        self.planner.push_back(Block {
            target: Some(target),
            remaining: self.config.block_duration,
            jog: true,
        });
        self.ack();
    }

    /// A deliberately small G-code model: only the words that move the
    /// simulated position are interpreted. Anything with an X, Y, or Z word
    /// is a motion block that takes `block_duration` to run.
    fn parse_motion(&mut self, line: &str) -> Result<Block, u32> {
        let upper = line.to_ascii_uppercase();
        let mut chars = upper.chars().filter(|c| !c.is_whitespace()).peekable();
        let mut axes: [Option<f64>; 3] = [None; 3];
        let mut absolute = self.absolute;
        let mut system = self.system;
        let mut machine = false;
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
                'G' if value == 53.0 => machine = true,
                'G' if value.fract() == 0.0 && (54.0..=59.0).contains(&value) => {
                    system = value as usize - 54;
                }
                // Grbl 1.1 refuses G59.1 to G59.3 with error:29.
                'G' if (59.0..60.0).contains(&value) => return Err(29),
                'X' => axes[0] = Some(value),
                'Y' => axes[1] = Some(value),
                'Z' => axes[2] = Some(value),
                _ => {}
            }
        }
        self.absolute = absolute;
        if system != self.system {
            // Like real firmware, the new offset is reported on the next status.
            self.wco_pending = true;
        }
        self.system = system;
        if axes.iter().all(Option::is_none) {
            return Ok(Block {
                target: None,
                remaining: Duration::ZERO,
                jog: false,
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
        let offset = if machine {
            [0.0; 3]
        } else {
            self.work_offsets[self.system]
        };
        for (i, axis) in axes.iter().enumerate() {
            if let Some(v) = axis {
                target[i] = if absolute {
                    *v + offset[i]
                } else {
                    base[i] + v
                };
            }
        }
        Ok(Block {
            target: Some(target),
            remaining: self.config.block_duration,
            jog: false,
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
    master: Arc<Mutex<Option<File>>>,
    slave: Mutex<Option<OwnedFd>>,
}

/// Opens a pseudo-terminal pair and points `link` at its slave side. The
/// simulator keeps the slave open itself so the master never sees EIO
/// while the sender is between connections.
fn open_pty(link: &Path) -> std::io::Result<(File, OwnedFd)> {
    let pty = nix::pty::openpty(None, None)?;
    let mut attrs = tcgetattr(pty.slave.as_fd())?;
    cfmakeraw(&mut attrs);
    tcsetattr(pty.slave.as_fd(), SetArg::TCSANOW, &attrs)?;
    let slave_path = nix::unistd::ttyname(pty.slave.as_fd())?;
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(&slave_path, link)?;
    let flags = OFlag::from_bits_truncate(fcntl(pty.master.as_fd(), FcntlArg::F_GETFL)?);
    fcntl(
        pty.master.as_fd(),
        FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK),
    )?;
    // The sender runs as a child process. If it inherited these descriptors,
    // closing them here would not hang up the port, as unplugging a cable does.
    for fd in [pty.master.as_fd(), pty.slave.as_fd()] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    Ok((File::from(pty.master), pty.slave))
}

impl Simulator {
    /// Starts a simulator whose serial port appears at `link`.
    pub fn start(config: SimConfig, link: impl AsRef<Path>) -> std::io::Result<Self> {
        let link = link.as_ref().to_path_buf();
        let (master, slave) = open_pty(&link)?;
        let master = Arc::new(Mutex::new(Some(master)));

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
            let master = master.clone();
            std::thread::spawn(move || run(master, state, stop))
        };
        Ok(Self {
            state,
            stop,
            thread: Some(thread),
            link,
            master,
            slave: Mutex::new(Some(slave)),
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

    /// Frees the port after the sender has closed it.
    ///
    /// Serial libraries open ports in exclusive mode, which the kernel
    /// clears when the last handle closes. The simulator keeps its own
    /// handle open, so without this a second sender could not open the
    /// port after the first exits (unless it runs as root).
    pub fn release_port(&self) {
        if let Some(slave) = self.slave.lock().unwrap().as_ref() {
            // SAFETY: TIOCNXCL takes no argument and the fd is owned by self.
            unsafe {
                libc::ioctl(slave.as_raw_fd(), libc::TIOCNXCL);
            }
        }
    }

    /// Unplugs the cable: the port disappears and anything the controller
    /// was about to say is lost. The machine itself keeps running whatever
    /// is in its planner.
    pub fn disconnect(&self) {
        let mut state = self.lock();
        state.out.clear();
        *self.master.lock().unwrap() = None;
        self.slave.lock().unwrap().take();
        let _ = std::fs::remove_file(&self.link);
    }

    /// Plugs the cable back in. Boards with a USB serial chip that toggles
    /// DTR (the Arduino based LongBoard) reset when the port is opened, so
    /// pass `reset: true` for those. The SuperLongBoard does not.
    pub fn reconnect(&self, reset: bool) -> std::io::Result<()> {
        let (master, slave) = open_pty(&self.link)?;
        let mut state = self.lock();
        if reset {
            state.soft_reset();
        }
        *self.master.lock().unwrap() = Some(master);
        *self.slave.lock().unwrap() = Some(slave);
        Ok(())
    }

    /// Makes the controller go silent: it ignores everything it receives
    /// and says nothing, though the port stays open. Motion continues.
    pub fn stall(&self, stalled: bool) {
        self.lock().stalled = stalled;
    }

    /// Loses the next `n` acknowledgements on the wire.
    pub fn drop_acks(&self, n: usize) {
        self.lock().drop_acks = n;
    }

    /// Emits raw bytes, such as line noise, ahead of the next response.
    pub fn inject(&self, bytes: &[u8]) {
        self.lock().out.extend_from_slice(bytes);
    }

    /// The active work offset, which status reports carry.
    pub fn work_offset(&self) -> [f64; 3] {
        let s = self.lock();
        s.work_offsets[s.system]
    }

    /// Sets the active work offset.
    pub fn set_work_offset(&self, offset: [f64; 3]) {
        let mut s = self.lock();
        let system = s.system;
        s.work_offsets[system] = offset;
        s.reports = 0;
    }

    /// The work offset of `system`, 54 to 59.
    pub fn work_offset_of(&self, system: u32) -> [f64; 3] {
        self.lock().work_offsets[system as usize - 54]
    }

    /// The active work coordinate system, 54 to 59.
    pub fn active_system(&self) -> u32 {
        54 + self.lock().system as u32
    }

    /// Soft limits in machine coordinates. A jog that would leave them is
    /// refused with `error:15`, as Grbl does with soft limits enabled.
    pub fn set_travel(&self, low: [f64; 3], high: [f64; 3]) {
        self.lock().travel = Some((low, high));
    }

    /// Moves the machine, as a jog or an earlier job would have.
    pub fn set_position(&self, position: [f64; 3]) {
        self.lock().position = position;
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

fn run(master: Arc<Mutex<Option<File>>>, state: Arc<Mutex<State>>, stop: Arc<AtomicBool>) {
    let tick = Duration::from_millis(1);
    let mut buf = [0u8; 4096];
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut incoming = Vec::new();
        if let Some(port) = master.lock().unwrap().as_mut() {
            loop {
                match port.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => incoming.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }
        let out = {
            let mut s = state.lock().unwrap();
            if !s.stalled {
                for b in incoming {
                    s.feed_byte(b);
                }
                s.process_lines();
            }
            let now = Instant::now();
            s.advance(now - last);
            last = now;
            if !s.stalled {
                // Space freed in the planner lets more lines through.
                s.process_lines();
            }
            let out = std::mem::take(&mut s.out);
            if s.stalled { Vec::new() } else { out }
        };
        let mut pending = &out[..];
        while !pending.is_empty() && !stop.load(Ordering::Relaxed) {
            let mut guard = master.lock().unwrap();
            let Some(port) = guard.as_mut() else {
                break;
            };
            match port.write(pending) {
                Ok(n) => pending = &pending[n..],
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    drop(guard);
                    std::thread::sleep(tick);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        std::thread::sleep(tick);
    }
}
