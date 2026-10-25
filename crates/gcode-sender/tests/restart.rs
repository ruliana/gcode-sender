//! Jobs survive the server process going away.

mod common;

use std::time::Duration;

use common::{Harness, zigzag};
use grbl_sim::SimConfig;

#[tokio::test]
async fn jobs_survive_restart() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let done = h.run(&zigzag(10)).await;
    h.wait_job_state(&done, "completed").await;
    let loaded = h.upload(&zigzag(3)).await;
    let loaded = loaded["id"].as_str().unwrap().to_string();
    let before = h.job(&done).await;

    h.restart_server().await;
    h.wait_ready().await;

    assert_eq!(h.job(&done).await, before);
    assert_eq!(h.job(&loaded).await["state"], "loaded");
    // A job uploaded before the restart can still be run.
    h.action(&loaded, "start").await;
    h.wait_job_state(&loaded, "completed").await;
}

#[tokio::test]
async fn one_damaged_record_does_not_take_the_others_down() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let good = h.upload(&zigzag(3)).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let damaged = h.upload(&zigzag(3)).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.kill_server().await;
    // A power cut can leave a record cut short.
    let record = h.data_dir().join("jobs").join(&damaged).join("job.json");
    std::fs::write(&record, b"{\"id\": \"").unwrap();

    h.restart_server().await;
    h.wait_ready().await;

    let (status, jobs) = h.get("/jobs").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let ids: Vec<&str> = jobs
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [good.as_str()]);
    assert_eq!(h.job(&good).await["state"], "loaded");
    h.action(&good, "start").await;
    h.wait_job_state(&good, "completed").await;
}

#[tokio::test]
async fn unfinished_upload_is_discarded_on_start() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    h.kill_server().await;
    // What a crash in the middle of an upload leaves behind.
    let staging = h
        .data_dir()
        .join("jobs")
        .join(".staging-6f0c2a3e-8d1b-4f7a-9c55-2b7e1d4a9e10");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("program.gcode"), "G1 X10 F600\n").unwrap();

    h.restart_server().await;
    h.wait_ready().await;

    assert!(!staging.exists());
    let (_, jobs) = h.get("/jobs").await;
    assert!(jobs.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn job_active_during_crash_is_interrupted_with_its_progress() {
    let mut h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(10))).await;
    let id = h.run(&zigzag(500)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(50))
        .await;
    // Give the periodic progress save a chance to run.
    tokio::time::sleep(Duration::from_millis(400)).await;

    h.kill_server().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.restart_server().await;

    let job = h.job(&id).await;
    assert_eq!(job["state"], "interrupted");
    assert!(job["reason"].as_str().unwrap().contains("server stopped"));
    // Progress is saved while running, so the record knows roughly where
    // the job was. It can lag the controller but never runs ahead of it.
    let acked = job["acked_lines"].as_u64().unwrap();
    assert!(acked > 0, "no progress was saved");
    let received = h
        .sim
        .received_lines()
        .iter()
        .filter(|l| l.starts_with('G'))
        .count() as u64;
    assert!(acked <= received);

    // The machine is free for new work after the restart.
    let m = h.wait_ready().await;
    assert!(m["active_job"].is_null());
    h.action(&id, "start").await;
    h.wait_job_state(&id, "completed").await;
}
