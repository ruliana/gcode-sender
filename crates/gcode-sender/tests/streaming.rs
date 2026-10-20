//! Uploading a program and streaming it to the controller end to end.

mod common;

use std::time::Duration;

use common::{Harness, zigzag};
use grbl_sim::SimConfig;
use reqwest::StatusCode;

/// The lines a correct sender delivers for `zigzag(n)`.
fn expected_lines(n: usize) -> Vec<String> {
    let mut lines = vec!["G21 G90".to_string()];
    for i in 1..=n {
        let y = if i % 2 == 0 { 1.0 } else { 0.0 };
        lines.push(format!("G1 X{:.3} Y{y:.3} F1000", i as f64 * 0.5));
    }
    lines.push("M2".into());
    lines
}

fn job_lines(h: &Harness) -> Vec<String> {
    h.sim
        .received_lines()
        .into_iter()
        .filter(|l| !l.starts_with('$'))
        .collect()
}

#[tokio::test]
async fn upload_creates_loaded_job() {
    let h = Harness::start(SimConfig::grbl()).await;
    let job = h.upload(&zigzag(10)).await;
    assert_eq!(job["state"], "loaded");
    assert_eq!(job["total_lines"], 12);
    assert_eq!(job["acked_lines"], 0);

    let id = job["id"].as_str().unwrap();
    assert_eq!(h.job(id).await, job);
    let (status, list) = h.get("/jobs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    // Nothing is sent until the job is started.
    assert!(job_lines(&h).is_empty());
}

#[tokio::test]
async fn upload_can_be_named() {
    let h = Harness::start(SimConfig::grbl()).await;
    let (status, job) = h.post_body("/jobs?name=pocket.nc", "G0 X1\n").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(job["name"], "pocket.nc");
}

#[tokio::test]
async fn rejects_program_with_nothing_to_send() {
    let h = Harness::start(SimConfig::grbl()).await;
    let (status, body) = h.post_body("/jobs", "; only a comment\n\n(and this)\n%\n").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("no lines"));
}

#[tokio::test]
async fn unknown_job_is_not_found() {
    let h = Harness::start(SimConfig::grbl()).await;
    let missing = "00000000-0000-4000-8000-000000000000";
    assert_eq!(h.get(&format!("/jobs/{missing}")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.post(&format!("/jobs/{missing}/start")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.get("/jobs/not-a-uuid").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn streams_file_end_to_end_on_grbl() {
    let h = Harness::start(SimConfig::grbl()).await;
    let id = h.run(&zigzag(500)).await;
    let job = h.wait_job_state(&id, "completed").await;

    assert_eq!(job["acked_lines"], 502);
    assert_eq!(job["total_lines"], 502);
    assert_eq!(job["line_errors"], serde_json::json!([]));
    assert!(job["finished_at_ms"].as_u64().unwrap() >= job["started_at_ms"].as_u64().unwrap());
    // Every line arrived once, in order, without comments.
    assert_eq!(job_lines(&h), expected_lines(500));
    assert_eq!(h.sim.position(), [250.0, 1.0, 0.0]);
    assert_eq!(h.sim.state(), "Idle");
    assert!(!h.sim.overflowed(), "RX buffer overflowed");
}

#[tokio::test]
async fn streams_file_end_to_end_on_grblhal() {
    let h = Harness::start(SimConfig::grblhal()).await;
    let id = h.run(&zigzag(500)).await;
    h.wait_job_state(&id, "completed").await;
    assert_eq!(job_lines(&h), expected_lines(500));
    assert!(!h.sim.overflowed(), "RX buffer overflowed");
}

#[tokio::test]
async fn character_counting_fills_but_never_overflows_buffer() {
    // Slow moves keep the planner full, so the sender must wait on acks.
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(10))).await;
    let id = h.run(&zigzag(200)).await;
    h.wait_job_state(&id, "completed").await;
    assert!(!h.sim.overflowed(), "RX buffer overflowed");
    // Streaming ahead of the planner is the point of character counting.
    let used = h.sim.max_rx_used();
    assert!(used > 64, "only {used} bytes of the RX buffer were ever used");
    assert!(used <= 128 - 8, "the reserve was not respected: {used}");
}

#[tokio::test]
async fn completes_only_after_motion_ends() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(30))).await;
    let id = h.run(&zigzag(40)).await;
    // All lines fit in the planner long before the moves finish.
    let job = h
        .wait_job(&id, "all lines acknowledged", |j| j["acked_lines"] == 42)
        .await;
    if job["state"] == "running" {
        assert_ne!(h.sim.state(), "Idle");
    }
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.sim.state(), "Idle");
}

#[tokio::test]
async fn reports_progress_while_running() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(5))).await;
    let id = h.run(&zigzag(300)).await;
    let job = h
        .wait_job(&id, "partial progress", |j| {
            let acked = j["acked_lines"].as_u64().unwrap();
            acked > 0 && acked < 302
        })
        .await;
    assert_eq!(job["state"], "running");
    assert!(job["last_acked_line"].as_u64().unwrap() > 0);
    let m = h
        .wait_machine("the machine to run", |m| m["status"]["state"] == "Run")
        .await;
    assert_eq!(m["active_job"]["id"], id.as_str());
    h.wait_job_state(&id, "completed").await;
    assert!(h.machine().await["active_job"].is_null());
}

#[tokio::test]
async fn only_one_job_runs_at_a_time() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(5))).await;
    let first = h.run(&zigzag(200)).await;
    let second = h.upload(&zigzag(5)).await;
    let (status, body) = h
        .post(&format!("/jobs/{}/start", second["id"].as_str().unwrap()))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains(&first));
    h.wait_job_state(&first, "completed").await;
}

#[tokio::test]
async fn completed_job_can_run_again() {
    let h = Harness::start(SimConfig::grbl()).await;
    let id = h.run(&zigzag(5)).await;
    h.wait_job_state(&id, "completed").await;
    h.action(&id, "start").await;
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 7);
    assert_eq!(job_lines(&h).len(), 14);
}

#[tokio::test]
async fn deletes_finished_job_but_not_active_one() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(5))).await;
    let id = h.run(&zigzag(200)).await;
    assert_eq!(h.delete(&format!("/jobs/{id}")).await.0, StatusCode::CONFLICT);
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.delete(&format!("/jobs/{id}")).await.0, StatusCode::NO_CONTENT);
    assert_eq!(h.get(&format!("/jobs/{id}")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn refuses_to_start_while_controller_is_in_alarm() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.trigger_alarm(1);
    h.wait_machine("the alarm", |m| m["alarm"] == 1 && m["ready"] == true)
        .await;
    let job = h.upload(&zigzag(3)).await;
    let (status, body) = h
        .post(&format!("/jobs/{}/start", job["id"].as_str().unwrap()))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn alarm_during_job_fails_it() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(10))).await;
    let id = h.run(&zigzag(300)).await;
    h.wait_job(&id, "some progress", |j| j["acked_lines"].as_u64() > Some(10))
        .await;
    h.sim.trigger_alarm(1);
    let job = h.wait_job_state(&id, "failed").await;
    assert!(job["reason"].as_str().unwrap().contains("alarm 1"));
    let sent = job_lines(&h).len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(job_lines(&h).len(), sent, "lines were sent after the alarm");
}

#[tokio::test]
async fn rejected_line_pauses_job_with_its_line_number() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.error_on("X5.000", 20);
    let id = h.run(&zigzag(30)).await;
    let job = h.wait_job_state(&id, "paused").await;
    // Five header lines precede the first move, so move 10 is line 15.
    let errors = job["line_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["code"], 20);
    assert_eq!(errors[0]["line"], 15);
    assert_eq!(errors[0]["text"], "G1 X5.000 Y1.000 F1000");
    assert!(job["reason"].as_str().unwrap().contains("line 15"));
    h.wait_machine("the hold", |m| m["status"]["state"] == "Hold").await;

    // The operator can decide to carry on.
    h.action(&id, "resume").await;
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 32);
}
