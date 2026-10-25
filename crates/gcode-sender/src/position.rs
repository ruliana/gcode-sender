//! Where the tool is, kept on disk so a restart does not lose it.
//!
//! The sender is the only thing that moves the machine. So the last position
//! it recorded is trusted over what the controller reports after a restart or
//! a reset, which can zero the reported position while the tool stays put. A
//! difference is not silently ignored: it becomes a check that the operator
//! answers with `POST /machine/position-check`.
//!
//! When the record is trusted (the default), the work zero is moved to keep
//! the tool where the record puts it. Every job's stored offset and paused
//! position is moved with it, so resuming still lands in the same place.

use serde::{Deserialize, Serialize};

/// Differences up to this many millimetres count as the same position.
pub const TOLERANCE: f64 = 0.01;

/// The tool's position as last seen, with the G54 offset in effect then.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Machine coordinates of the tool.
    pub machine: [f64; 3],
    /// The G54 offset, in machine coordinates.
    pub work_offset: [f64; 3],
}

/// A recorded position that the controller disagrees with, waiting for the
/// operator's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// The position the sender had recorded.
    pub recorded: [f64; 3],
    /// The position the controller reports now.
    pub reported: [f64; 3],
    /// The G54 offset the record was made under.
    pub previous_offset: [f64; 3],
    /// The G54 offset in effect now.
    pub offset: [f64; 3],
    /// Whether the work zero has been moved to keep the tool where the record
    /// puts it. Off, jobs wait until the operator answers.
    pub adjusted: bool,
    /// What to ask the operator.
    pub question: String,
}

/// What `<data>/position.json` holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub record: Option<Record>,
    pub check: Option<Check>,
}

pub fn differs(a: [f64; 3], b: [f64; 3]) -> bool {
    a.iter().zip(b).any(|(x, y)| (x - y).abs() > TOLERANCE)
}

/// How far `to` is from `from`, per axis.
pub fn delta(from: [f64; 3], to: [f64; 3]) -> [f64; 3] {
    [to[0] - from[0], to[1] - from[1], to[2] - from[2]]
}

pub fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn same_offset(a: [f64; 3], b: [f64; 3]) -> bool {
    !differs(a, b)
}

pub fn fmt(p: [f64; 3]) -> String {
    format!("X{:.3} Y{:.3} Z{:.3}", p[0], p[1], p[2])
}
