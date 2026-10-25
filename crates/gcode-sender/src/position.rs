//! Where the tool is, kept on disk so a restart does not lose it.
//!
//! The sender is the only thing that moves the machine. So the last position
//! it recorded is trusted over what the controller reports after a restart or
//! a reset, which can zero the reported position while the tool stays put. A
//! difference is not silently ignored: it becomes a check that the operator
//! answers with `POST /machine/position-check`.
//!
//! When the record is trusted (the default), every work zero (G54 to G59) is
//! moved by the same amount, so each system still reads the tool where it was
//! recorded. Stored jobs' zeros and paused positions move with them, so a
//! resume still lands in the same place.

use serde::{Deserialize, Serialize};

use crate::work::Zeros;

/// Differences up to this many millimetres count as the same position.
pub const TOLERANCE: f64 = 0.01;

/// The tool's position as last seen, with every work zero at that time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Machine coordinates of the tool.
    pub machine: [f64; 3],
    /// Each work coordinate system's zero, in machine coordinates.
    pub zeros: Zeros,
}

/// A recorded position that the controller disagrees with, waiting for the
/// operator's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// The position the sender had recorded.
    pub recorded: [f64; 3],
    /// The position the controller reports now.
    pub reported: [f64; 3],
    /// The zeros the record was made under.
    pub previous_zeros: Zeros,
    /// The zeros in effect now.
    pub zeros: Zeros,
    /// Whether the zeros have been moved to keep the tool where the record
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

/// Whether two sets of zeros are the same, system by system.
pub fn zeros_differ(a: Zeros, b: Zeros) -> bool {
    a.iter().zip(b).any(|(x, y)| differs(*x, y))
}

/// How far `to` is from `from`, per axis.
pub fn delta(from: [f64; 3], to: [f64; 3]) -> [f64; 3] {
    [to[0] - from[0], to[1] - from[1], to[2] - from[2]]
}

pub fn add(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// Every zero moved by the same amount.
pub fn shift_zeros(zeros: Zeros, d: [f64; 3]) -> Zeros {
    zeros.map(|z| add(z, d))
}

pub fn fmt(p: [f64; 3]) -> String {
    format!("X{:.3} Y{:.3} Z{:.3}", p[0], p[1], p[2])
}
