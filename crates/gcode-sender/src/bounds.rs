//! Optional machine boundaries. When set, a job is checked against them
//! before any of it is sent, so a program that would drive an axis past its
//! limit never reaches the motors.

use serde::{Deserialize, Serialize};

use crate::gcode::{Interpreter, Unsupported};
use crate::program::ProgramLine;
use crate::work::WorkOffsets;

/// Allowed machine-coordinate range per axis, in millimetres. An axis left
/// out is not checked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bounds {
    pub x: Option<[f64; 2]>,
    pub y: Option<[f64; 2]>,
    pub z: Option<[f64; 2]>,
}

impl Bounds {
    pub fn validate(&self) -> Result<(), String> {
        for (name, range) in [("x", self.x), ("y", self.y), ("z", self.z)] {
            if let Some([min, max]) = range
                && !(min.is_finite() && max.is_finite() && min <= max)
            {
                return Err(format!("{name} must be [min, max] with min <= max"));
            }
        }
        Ok(())
    }

    /// Checks a single destination, in machine coordinates. Axes that are
    /// `None` are not being moved.
    pub fn check_point(&self, target: [Option<f64>; 3]) -> Result<(), String> {
        for (axis, name) in AXES.iter().enumerate() {
            let (Some(at), Some([min, max])) = (target[axis], self.range(axis)) else {
                continue;
            };
            if at < min - EPSILON || at > max + EPSILON {
                return Err(format!(
                    "the jog moves {name} to {at:.3}, outside the boundary {min} to {max}"
                ));
            }
        }
        Ok(())
    }

    fn range(&self, axis: usize) -> Option<[f64; 2]> {
        [self.x, self.y, self.z][axis]
    }
}

const AXES: [&str; 3] = ["X", "Y", "Z"];
/// Slack for floating point noise in arc and unit conversions.
const EPSILON: f64 = 1e-6;

/// Runs the whole program from `position` and returns a message naming the
/// first line that leaves the boundaries.
///
/// `position` is in machine coordinates, in millimetres, as the controller
/// reports it. The program starts in the active work coordinate system of
/// `work`, and follows any G54 to G59 it selects.
pub fn check(
    bounds: &Bounds,
    lines: &[ProgramLine],
    position: [f64; 3],
    work: &WorkOffsets,
) -> Result<(), String> {
    let mut machine = Interpreter::new(position, work.zeros, work.system);
    for line in lines {
        let extent = machine.step(&line.text).map_err(|Unsupported(what)| {
            format!(
                "line {} uses {what}, which cannot be checked against the boundaries",
                line.number
            )
        })?;
        let Some(extent) = extent else { continue };
        for (axis, name) in AXES.iter().enumerate() {
            let Some([min, max]) = bounds.range(axis) else {
                continue;
            };
            let (low, high) = (extent.min[axis], extent.max[axis]);
            // A comparison with NaN is false, so a NaN would pass every check
            // below. Refuse it outright.
            if !low.is_finite() || !high.is_finite() {
                return Err(format!(
                    "line {} ({}) moves {name} to a position that is not a finite number",
                    line.number, line.text
                ));
            }
            let bad = if low < min - EPSILON {
                low
            } else if high > max + EPSILON {
                high
            } else {
                continue;
            };
            return Err(format!(
                "line {} ({}) moves {name} to {bad:.3}, outside the boundary {min} to {max}",
                line.number, line.text
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program::prepare;

    fn limits() -> Bounds {
        Bounds {
            x: Some([-100.0, 100.0]),
            y: Some([-100.0, 100.0]),
            z: None,
        }
    }

    fn work() -> WorkOffsets {
        WorkOffsets {
            system: 54,
            zeros: [[0.0; 3]; 6],
        }
    }

    #[test]
    fn a_program_inside_the_limits_passes() {
        let program = prepare("G0 X10 Y10\nG1 X-50 Y20 F300\nG2 X0 Y0 I50 J0");
        assert_eq!(check(&limits(), &program, [0.0; 3], &work()), Ok(()));
    }

    #[test]
    fn a_move_past_the_limit_names_its_line() {
        let program = prepare("G0 X10\nG1 X150");
        let err = check(&limits(), &program, [0.0; 3], &work()).unwrap_err();
        assert!(err.starts_with("line 2 (G1 X150)"), "{err}");
    }

    #[test]
    fn a_coordinate_too_large_to_be_a_number_is_refused() {
        // Digits this long read as infinity, which no boundary can contain.
        let program = prepare(&format!("G1 X{}", "9".repeat(400)));
        assert!(check(&limits(), &program, [0.0; 3], &work()).is_err());
    }
}
