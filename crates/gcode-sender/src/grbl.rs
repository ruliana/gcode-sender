//! Parsing of what GRBL-family controllers print on the serial line.

use serde::Serialize;

/// Realtime command bytes. These bypass the RX buffer and are never counted.
pub mod realtime {
    pub const STATUS: u8 = b'?';
    pub const FEED_HOLD: u8 = b'!';
    pub const CYCLE_START: u8 = b'~';
    pub const SOFT_RESET: u8 = 0x18;
    pub const JOG_CANCEL: u8 = 0x85;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FirmwareKind {
    Grbl,
    Grblhal,
}

#[derive(Debug, PartialEq)]
pub enum Response {
    Ok,
    Error(u32),
    Alarm(u32),
    /// The startup banner, printed after power-on or a soft reset.
    Banner {
        kind: FirmwareKind,
        version: String,
    },
    Status(Status),
    /// A bracketed message such as `[VER:...]`, `[OPT:...]` or `[MSG:...]`.
    Message {
        tag: String,
        body: String,
    },
    Other(String),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Status {
    /// The machine state, such as `Idle`, `Run`, `Hold` or `Alarm`.
    pub state: String,
    /// The substate after the colon, as in `Hold:0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub substate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_position: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_position: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spindle_speed: Option<f64>,
    /// The active work coordinate system's offset in machine coordinates, as
    /// last reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_offset: Option<Vec<f64>>,
}

impl Status {
    /// True when a feed hold has fully decelerated (`Hold:0`).
    pub fn hold_complete(&self) -> bool {
        self.state == "Hold" && self.substate == Some(0)
    }
}

pub fn parse(line: &str) -> Response {
    let line = line.trim();
    if line == "ok" {
        return Response::Ok;
    }
    if let Some(code) = line.strip_prefix("error:") {
        return Response::Error(code.trim().parse().unwrap_or(0));
    }
    if let Some(code) = line.strip_prefix("ALARM:") {
        return Response::Alarm(code.trim().parse().unwrap_or(0));
    }
    if let Some(body) = line.strip_prefix('<').and_then(|l| l.strip_suffix('>')) {
        return Response::Status(parse_status(body));
    }
    if let Some(body) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
        let (tag, body) = body.split_once(':').unwrap_or((body, ""));
        return Response::Message {
            tag: tag.to_string(),
            body: body.to_string(),
        };
    }
    if let Some(banner) = parse_banner(line) {
        return banner;
    }
    Response::Other(line.to_string())
}

fn parse_banner(line: &str) -> Option<Response> {
    let lower = line.to_ascii_lowercase();
    let (kind, rest) = match lower.strip_prefix("grblhal ") {
        Some(rest) => (FirmwareKind::Grblhal, rest),
        None => (FirmwareKind::Grbl, lower.strip_prefix("grbl ")?),
    };
    let version = rest.split_whitespace().next()?.to_string();
    Some(Response::Banner { kind, version })
}

/// A `[G55:x,y,z]` line from `$#`: which work coordinate system's zero it
/// gives, as an index into [`crate::work::SYSTEMS`], and the zero.
pub fn work_zero_message(tag: &str, body: &str) -> Option<(usize, [f64; 3])> {
    let index = crate::work::SYSTEMS
        .iter()
        .position(|s| tag.eq_ignore_ascii_case(&format!("G{s}")))?;
    let v = parse_floats(body);
    (v.len() >= 3).then(|| (index, [v[0], v[1], v[2]]))
}

/// The active work coordinate system from a `[GC:...]` line from `$G`, such
/// as `G0 G55 G17 G21 ...`.
pub fn active_system_message(body: &str) -> Option<u32> {
    body.split_whitespace().find_map(|word| {
        let number: u32 = word.strip_prefix('G')?.parse().ok()?;
        crate::work::index(number).map(|_| number)
    })
}

fn parse_floats(s: &str) -> Vec<f64> {
    s.split(',').filter_map(|v| v.trim().parse().ok()).collect()
}

fn parse_status(body: &str) -> Status {
    let mut fields = body.split('|');
    let mut status = Status::default();
    let state = fields.next().unwrap_or_default();
    match state.split_once(':') {
        Some((name, sub)) => {
            status.state = name.to_string();
            status.substate = sub.parse().ok();
        }
        None => status.state = state.to_string(),
    }
    for field in fields {
        let Some((key, value)) = field.split_once(':') else {
            continue;
        };
        match key {
            "MPos" => status.machine_position = Some(parse_floats(value)),
            "WPos" => status.work_position = Some(parse_floats(value)),
            "WCO" => status.work_offset = Some(parse_floats(value)),
            "FS" | "F" => {
                let v = parse_floats(value);
                status.feed_rate = v.first().copied();
                status.spindle_speed = v.get(1).copied();
            }
            _ => {}
        }
    }
    status
}

/// Reads the RX buffer size from an `[OPT:...]` body. The third field is
/// the serial RX buffer size on both Grbl 1.1 and grblHAL.
pub fn rx_buffer_from_opt(body: &str) -> Option<usize> {
    body.split(',').nth(2)?.trim().parse().ok()
}

/// What an `error:N` reply means, from the Grbl 1.1 documentation.
pub fn error_message(code: u32) -> &'static str {
    match code {
        1 => "expected a command letter",
        2 => "bad number format",
        3 => "invalid $ statement",
        4 => "negative value for an expected positive value",
        5 => "setting is disabled",
        8 => "not allowed unless the machine is idle",
        9 => "G-code is locked out during an alarm or jog",
        10 => "homing is not enabled",
        11 => "line overflow",
        14 => "line length exceeded",
        15 => "jog target exceeds machine travel",
        16 => "invalid jog command",
        20 => "unsupported or invalid G-code command",
        21 => "more than one command from the same modal group",
        22 => "feed rate has not been set",
        23 => "a command requires an integer value",
        24 => "two commands both need axis words",
        25 => "a G-code word was repeated",
        26 => "a command requires axis words",
        27 => "line number is invalid",
        28 => "a P or L value is missing",
        29 => "G59.x work coordinate systems are not supported",
        30 => "G53 is only valid with G0 and G1",
        31 => "axis words exist but no command uses them",
        32 => "arc needs at least one in-plane axis word",
        33 => "motion target is invalid",
        34 => "arc radius is invalid",
        35 => "arc needs at least one in-plane offset word",
        36 => "unused value words in the block",
        _ => "unknown error",
    }
}

/// What an `ALARM:N` means, from the Grbl 1.1 documentation.
pub fn alarm_message(code: u32) -> &'static str {
    match code {
        1 => "hard limit triggered; position may be lost",
        2 => "the motion target exceeds the machine's soft limits",
        3 => "reset while in motion; position may be lost",
        4 => "probe failed: not in the expected starting state",
        5 => "probe failed: no contact",
        6 => "homing failed: reset during the cycle",
        7 => "homing failed: door opened",
        8 => "homing failed: could not clear the limit switch",
        9 => "homing failed: could not find the limit switch",
        _ => "unknown alarm",
    }
}

/// Reads the planner block count from an `[OPT:...]` body. It is the second
/// field on both Grbl 1.1 and grblHAL.
pub fn planner_blocks_from_opt(body: &str) -> Option<usize> {
    body.split(',').nth(1)?.trim().parse().ok()
}
