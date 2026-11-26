//! Damaged files in the data directory, as a power cut or a bad disk leaves
//! them: whatever the bytes, the store must start, list, recover and read
//! without panicking.

#![no_main]

use gcode_sender::store::JobStore;
use libfuzzer_sys::fuzz_target;

/// A well-formed job id, so the store looks in the job directory.
const JOB: &str = "3f2c9a1e-5b7d-4c8e-9a0f-1234567890ab";

fuzz_target!(|data: &[u8]| {
    let dir = tempfile::tempdir().expect("temp dir");
    let job_dir = dir.path().join("jobs").join(JOB);
    std::fs::create_dir_all(&job_dir).expect("job dir");
    std::fs::write(job_dir.join("job.json"), data).expect("job file");
    std::fs::write(dir.path().join("position.json"), data).expect("position file");
    std::fs::write(dir.path().join("settings.json"), data).expect("settings file");
    std::fs::write(dir.path().join("bounds.json"), data).expect("bounds file");

    let Ok(store) = JobStore::open(dir.path()) else {
        return;
    };
    let _ = store.list();
    let _ = store.get(JOB);
    let _ = store.program(JOB);
    let _ = store.position();
    let _ = store.settings();
    let _ = store.bounds();
    let _ = store.recover();
});
