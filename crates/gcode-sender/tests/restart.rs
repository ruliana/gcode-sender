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
