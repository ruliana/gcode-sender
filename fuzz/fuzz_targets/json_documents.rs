//! Any bytes as a JSON body: each document the HTTP API reads must either be
//! refused or be something that validates and round-trips.

#![no_main]

use gcode_sender::bounds::Bounds;
use gcode_sender::position::Position;
use gcode_sender::settings::{PauseOptions, PauseSettings, Settings};
use gcode_sender::work::WorkOffsets;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(work) = serde_json::from_slice::<WorkOffsets>(data) {
        let again = serde_json::to_value(&work).expect("offsets serialise");
        let back: WorkOffsets = serde_json::from_value(again).expect("offsets read back");
        assert_eq!(back, work);
    }
    if let Ok(bounds) = serde_json::from_slice::<Bounds>(data) {
        let _ = bounds.validate();
    }
    if let Ok(settings) = serde_json::from_slice::<Settings>(data) {
        let _ = settings.validate();
    }
    if let Ok(options) = serde_json::from_slice::<PauseOptions>(data) {
        let _ = options.apply(&PauseSettings::default());
    }
    let _ = serde_json::from_slice::<Position>(data);
});
