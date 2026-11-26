//! Any bytes as a G-code file: preparing it, interpreting every line, and
//! checking it against boundaries must not panic, and a check that passes
//! must not have hidden a move that is not a number.

#![no_main]

use gcode_sender::bounds::{self, Bounds};
use gcode_sender::gcode::Interpreter;
use gcode_sender::program::prepare;
use gcode_sender::work::WorkOffsets;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    let lines = prepare(&source);

    let mut machine = Interpreter::new([0.0; 3], [[0.0; 3]; 6], 54);
    for line in &lines {
        if let Ok(Some(extent)) = machine.step(&line.text) {
            assert!(
                extent.min.iter().chain(&extent.max).all(|v| !v.is_nan()),
                "NaN in the extent of {:?}",
                line.text
            );
        }
    }

    let limits = Bounds {
        x: Some([-100.0, 100.0]),
        y: Some([-100.0, 100.0]),
        z: Some([-50.0, 0.0]),
    };
    let work = WorkOffsets {
        system: 54,
        zeros: [[0.0; 3]; 6],
    };
    let _ = bounds::check(&limits, &lines, [0.0; 3], &work);
});
