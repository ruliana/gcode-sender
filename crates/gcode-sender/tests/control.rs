//! Pausing, resuming, and stopping a running job.

mod common;

use std::time::Duration;

use common::{Harness, zigzag};
use grbl_sim::SimConfig;
use reqwest::StatusCode;

/// Slow enough that a job is still running when the test acts on it.
fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(20))
}

async fn running_job(h: &Harness, moves: usize) -> String {
    let id = h.run(&zigzag(moves)).await;
    h.wait_job(&id, "the job to make progress", |j| {
        j["acked_lines"].as_u64() > Some(5)
    })
    .await;
    id
}

#[tokio::test]
async fn pause_holds_machine_and_stops_sending() {
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 300).await;

    let job = h.hold(&id).await;
    assert_eq!(job["state"], "paused");
    h.wait_machine("the hold to settle", |m| {
        m["status"]["state"] == "Hold" && m["status"]["substate"] == 0
    })
    .await;
    assert!(h.sim.realtime_bytes().contains(&b'!'));

    let position = h.sim.position();
    let received = h.sim.received_lines().len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.sim.position(), position, "the machine moved while paused");
    assert_eq!(
        h.sim.received_lines().len(),
        received,
        "lines were sent while paused"
    );
    assert_eq!(h.job(&id).await["state"], "paused");
}

#[tokio::test]
async fn resume_continues_to_completion() {
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 100).await;
    h.hold(&id).await;
    h.wait_machine("the hold to settle", |m| m["status"]["substate"] == 0)
        .await;

    let job = h.action(&id, "resume").await;
    assert_eq!(job["state"], "running");
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 102);
    assert_eq!(h.sim.position(), [50.0, 1.0, 0.0]);
    assert!(!h.sim.overflowed());
}

#[tokio::test]
async fn resume_right_after_pause_is_not_lost() {
    // Cycle start is ignored while a hold is still decelerating; the
    // sender has to wait for the hold to settle before sending it.
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 60).await;
    h.hold(&id).await;
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn pause_and_resume_many_times() {
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 150).await;
    for _ in 0..5 {
        h.hold(&id).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        h.action(&id, "resume").await;
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 152);
    assert_eq!(
        h.sim
            .received_lines()
            .iter()
            .filter(|l| l.starts_with("G1"))
            .count(),
        150
    );
}

#[tokio::test]
async fn stop_halts_without_losing_position() {
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 300).await;

    h.action(&id, "stop").await;
    let job = h.wait_job_state(&id, "stopped").await;
    assert!(job["acked_lines"].as_u64().unwrap() < 302);
    assert!(job["finished_at_ms"].is_u64());

    // Hold first, then reset: the machine decelerates and keeps position.
    let rt = h.sim.realtime_bytes();
    let hold = rt.iter().position(|&b| b == b'!').expect("feed hold sent");
    let reset = rt.iter().position(|&b| b == 0x18).expect("soft reset sent");
    assert!(hold < reset);
    assert_eq!(h.sim.soft_resets(), 1);
    assert_eq!(h.sim.alarm(), None, "stopping raised an alarm");

    // The sender re-identifies the controller after the reset, but sends
    // nothing more from the job.
    let program_lines = || {
        h.sim
            .received_lines()
            .iter()
            .filter(|l| !l.starts_with('$'))
            .count()
    };
    let position = h.sim.position();
    let received = program_lines();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.sim.position(),
        position,
        "the machine kept moving after stop"
    );
    assert_eq!(program_lines(), received, "lines were sent after stop");

    let m = h.wait_ready().await;
    assert!(m["active_job"].is_null());
    assert!(m["alarm"].is_null());
}

#[tokio::test]
async fn stop_while_paused() {
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 300).await;
    h.hold(&id).await;
    h.wait_machine("the hold to settle", |m| m["status"]["substate"] == 0)
        .await;
    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
    assert_eq!(h.sim.alarm(), None);
}

#[tokio::test]
async fn new_job_runs_cleanly_after_stop() {
    let h = Harness::start(slow()).await;
    let id = running_job(&h, 300).await;
    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
    h.wait_ready().await;

    h.sim.set_block_duration(Duration::from_millis(1));
    let before = h.sim.received_lines().len();
    let next = h.run("G90\nG0 X7 Y8 Z-1\n").await;
    h.wait_job_state(&next, "completed").await;
    let after: Vec<_> = h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect();
    assert_eq!(after, ["G90", "G0 X7 Y8 Z-1"]);
    assert_eq!(h.sim.position(), [7.0, 8.0, -1.0]);
}

#[tokio::test]
async fn control_actions_check_job_state() {
    let h = Harness::start(slow()).await;
    let loaded = h.upload(&zigzag(5)).await;
    let loaded = loaded["id"].as_str().unwrap();
    for action in ["pause", "resume", "stop"] {
        let (status, body) = h.post(&format!("/jobs/{loaded}/{action}")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
    }

    let id = running_job(&h, 300).await;
    let (status, _) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::CONFLICT, "resume while running");
    h.hold(&id).await;
    let (status, _) = h.post(&format!("/jobs/{id}/pause")).await;
    assert_eq!(status, StatusCode::CONFLICT, "pause while paused");
    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
    let (status, _) = h.post(&format!("/jobs/{id}/stop")).await;
    assert_eq!(status, StatusCode::CONFLICT, "stop while stopped");
}
