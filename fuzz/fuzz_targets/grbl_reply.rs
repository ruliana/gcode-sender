//! Any bytes from the serial line: parsing a reply, and the helpers that
//! read numbers out of its bracketed messages, must not panic.

#![no_main]

use gcode_sender::grbl::{self, Response};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let line = String::from_utf8_lossy(data);
    match grbl::parse(&line) {
        Response::Status(status) => {
            // Each position that was read has all three axes, or none.
            for position in [&status.machine_position, &status.work_position, &status.work_offset]
                .into_iter()
                .flatten()
            {
                assert!(!position.is_empty());
            }
        }
        Response::Message { tag, body } => {
            let _ = grbl::work_zero_message(&tag, &body);
            let _ = grbl::active_system_message(&body);
            let _ = grbl::rx_buffer_from_opt(&body);
            let _ = grbl::planner_blocks_from_opt(&body);
        }
        Response::Error(code) => {
            let _ = grbl::error_message(code);
        }
        Response::Alarm(code) => {
            let _ = grbl::alarm_message(code);
        }
        Response::Ok | Response::Banner { .. } | Response::Other(_) => {}
    }
});
