//! Parsing of what GRBL-family controllers print on the serial line.

use serde::Serialize;

/// Realtime command bytes. These bypass the RX buffer and are never counted.
pub mod realtime {
    pub const STATUS: u8 = b'?';
    pub const FEED_HOLD: u8 = b'!';
    pub const CYCLE_START: u8 = b'~';
    pub const SOFT_RESET: u8 = 0x18;
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
    Banner { kind: FirmwareKind, version: String },
    Status(Status),
    /// A bracketed message such as `[VER:...]`, `[OPT:...]` or `[MSG:...]`.
    Message { tag: String, body: String },
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
    #[serde(skip)]
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
    let (kind, rest) = if let Some(rest) = lower.strip_prefix("grblhal ") {
        (FirmwareKind::Grblhal, rest)
    } else if let Some(rest) = lower.strip_prefix("grbl ") {
        (FirmwareKind::Grbl, rest)
    } else {
        return None;
    };
    let version = rest.split_whitespace().next()?.to_string();
    Some(Response::Banner { kind, version })
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
