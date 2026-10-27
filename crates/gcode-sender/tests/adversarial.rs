//! Hostile conditions: a link that is slow, split, lossy or noisy, a
//! controller that resets or ignores us, clients that vanish mid-request,
//! and a disk that was damaged between runs.
//!
//! Each test states the behaviour the sender must keep, and fails if a
//! fault leaves a job hung, a record wrong, or the server unreachable.

mod common;

use std::time::Duration;

use common::{Harness, program_lines, wait_for, zigzag, zigzag_lines};
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Slow enough that the controller's planner is full while a job runs.
fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(20))
}

/// The `host:port` the server listens on for this machine.
fn authority(h: &Harness) -> String {
    h.base.trim_start_matches("http://").to_string()
}

/// Sends raw bytes to the server and returns what it answers, if anything.
async fn raw(h: &Harness, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(authority(h)).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut answer)).await;
    String::from_utf8_lossy(&answer).into_owned()
}

fn post_head(path: &str, length: usize) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    )
}

// ---- A link that is slow, split, lossy or noisy --------------------------

#[tokio::test]
async fn responses_split_into_single_bytes_are_reassembled() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_chunked(true);
    let id = h.run(&zigzag(20)).await;
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 22);
    assert_eq!(program_lines(&h.sim), zigzag_lines(20));
    assert!(!h.sim.overflowed());
}

#[tokio::test]
async fn slow_link_is_not_mistaken_for_a_dead_one() {
    // Silence timeout is 1.5 s in the harness; replies arrive 400 ms late.
    let h = Harness::start(slow()).await;
    h.sim.set_latency(Duration::from_millis(400));
    let id = h.run(&zigzag(30)).await;
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 32);
    assert_eq!(program_lines(&h.sim), zigzag_lines(30));
    assert_eq!(h.machine().await["connection"], "connected");
}

#[tokio::test]
async fn lost_status_reports_do_not_stall_the_job() {
    let h = Harness::start(slow()).await;
    // A burst of 400 ms of lost replies, well inside the silence timeout.
    h.sim.lose_status_reports(8);
    let id = h.run(&zigzag(60)).await;
    h.wait_job_state(&id, "completed").await;
    assert_eq!(program_lines(&h.sim), zigzag_lines(60));
}

#[tokio::test]
async fn malformed_status_reports_are_ignored() {
    let h = Harness::start(slow()).await;
    let id = h.run(&zigzag(60)).await;
    for junk in [
        "<Run|MPos:",
        "<Run|MPos:abc,def,ghi|FS:|WCO:>",
        "<|||>",
        "<Hold:x|MPos:1,2>",
        "<Alarm:99999999999999999999999>",
        "<>",
        "<Run|MPos:1,2,3,4,5,6,7,8,9>",
        "[GC:",
        "[]",
    ] {
        h.sim.inject(format!("{junk}\r\n").as_bytes());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.wait_job_state(&id, "completed").await;
    assert_eq!(program_lines(&h.sim), zigzag_lines(60));
}

// ---- A controller that resets or misbehaves on its own -------------------

#[tokio::test]
async fn operator_reset_mid_job_interrupts_it_and_resume_completes() {
    let h = Harness::start(slow()).await;
    let id = h.run(&zigzag(100)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;

    // Resetting in motion raises ALARM:3, which fails the job.
    h.sim.press_reset();
    let job = h.wait_job_state(&id, "failed").await;
    assert!(
        job["reason"]
            .as_str()
            .unwrap()
            .contains("position may be lost")
    );
    h.wait_machine("the reset alarm", |m| m["alarm"] == 3 && m["ready"] == true)
        .await;

    let (status, _) = h.post("/machine/unlock").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_idle().await;
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
    for line in &zigzag_lines(100) {
        assert!(program_lines(&h.sim).contains(line), "{line} never sent");
    }
}

#[tokio::test]
async fn stop_ends_even_when_the_feed_hold_never_settles() {
    let h = Harness::start(slow()).await;
    h.sim.set_hold_decel(Duration::from_secs(60));
    let id = h.run(&zigzag(200)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(20))
        .await;

    h.action(&id, "stop").await;
    let job = h.wait_job_state(&id, "stopped").await;
    assert!(job["reason"].as_str().unwrap().contains("stopped"));
    h.wait_machine("the machine to come back", |m| m["ready"] == true)
        .await;
}

#[tokio::test]
async fn line_longer_than_the_rx_buffer_is_refused_not_left_hanging() {
    // A 128 byte buffer cannot take this line even when empty. Sending it
    // anyway would leave the job waiting for an acknowledgement forever.
    let h = Harness::start(SimConfig::grbl()).await;
    let long = format!("G1 X1.000 Y{} F1000", "0".repeat(200));
    let id = h.upload(&format!("G21\n{long}\nM2\n")).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = h.post(&format!("/jobs/{id}/start")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"].as_str().unwrap().contains("line 2"));
    assert_eq!(h.job(&id).await["state"], "loaded");
    assert!(program_lines(&h.sim).is_empty());
}

#[tokio::test]
async fn garbage_before_the_banner_does_not_break_identification() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    h.sim.inject(b"\xff\xfe\x00\x01garbage without a newline");
    h.wait_ready().await;
    let id = h.run(&zigzag(5)).await;
    h.wait_job_state(&id, "completed").await;
}

// ---- Concurrency and clients that vanish ----------------------------------

#[tokio::test]
async fn racing_start_requests_start_the_job_exactly_once() {
    let h = Harness::start(slow()).await;
    let id = h.upload(&zigzag(30)).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/jobs/{id}/start");
    let attempts = (0..10).map(|_| h.post(&path));
    let answers = futures::future::join_all(attempts).await;

    let started = answers
        .iter()
        .filter(|(s, _)| *s == StatusCode::ACCEPTED)
        .count();
    assert_eq!(
        started,
        1,
        "{:?}",
        answers.iter().map(|a| a.0).collect::<Vec<_>>()
    );
    assert!(
        answers
            .iter()
            .all(|(s, _)| { matches!(*s, StatusCode::ACCEPTED | StatusCode::CONFLICT) })
    );
    h.wait_job_state(&id, "completed").await;
    assert_eq!(program_lines(&h.sim), zigzag_lines(30));
}

#[tokio::test]
async fn upload_abandoned_halfway_creates_no_job() {
    let h = Harness::start(SimConfig::grbl()).await;
    let mut request = post_head("/jobs?name=half.nc", 1000).into_bytes();
    request.extend_from_slice(b"G1 X1 F100\n");
    let answer = raw(&h, &request).await;
    assert!(
        answer.is_empty() || !answer.contains("201 Created"),
        "{answer}"
    );
    let (status, list) = h.get("/jobs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list, Value::Array(vec![]));
    assert_eq!(h.get("/health").await.0, StatusCode::OK);
}

#[tokio::test]
async fn client_that_hangs_up_after_start_does_not_stop_the_job() {
    let h = Harness::start(slow()).await;
    let id = h.upload(&zigzag(40)).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let request =
        format!("POST /jobs/{id}/start HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
    let mut stream = TcpStream::connect(authority(&h)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    // The server has the request once the job starts; then the client goes.
    h.wait_job(&id, "the job to start", |j| j["state"] == "running")
        .await;
    drop(stream);

    h.wait_job_state(&id, "completed").await;
    assert_eq!(program_lines(&h.sim), zigzag_lines(40));
}

#[tokio::test]
async fn idle_connections_do_not_block_other_clients() {
    let h = Harness::start(SimConfig::grbl()).await;
    let mut silent = Vec::new();
    for _ in 0..50 {
        silent.push(TcpStream::connect(authority(&h)).await.unwrap());
    }
    let (status, _) = tokio::time::timeout(Duration::from_secs(3), h.get("/machine"))
        .await
        .expect("a silent connection blocked the server");
    assert_eq!(status, StatusCode::OK);
    let id = h.run(&zigzag(5)).await;
    h.wait_job_state(&id, "completed").await;
    drop(silent);
}

#[tokio::test]
async fn event_subscriber_that_stops_reading_does_not_stall_the_job() {
    let h = Harness::start(SimConfig::grbl()).await;
    let _events = h.events().await;
    let id = h.run(&zigzag(200)).await;
    h.wait_job_state(&id, "completed").await;
}

// ---- Damage left behind by a crash or a power cut -------------------------

#[tokio::test]
async fn torn_job_record_is_left_out_after_restart() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let good = h.run(&zigzag(5)).await;
    h.wait_job_state(&good, "completed").await;
    let torn = h.upload(&zigzag(5)).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    h.kill_server().await;
    let record = h.data_dir().join("jobs").join(&torn).join("job.json");
    std::fs::write(&record, b"{\"id\": \"").unwrap();
    h.restart_server().await;
    h.wait_ready().await;

    let (status, list) = h.get("/jobs").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [good.as_str()]);
    let (status, _) = h.get(&format!("/jobs/{torn}")).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a torn record was served as healthy"
    );
    assert_eq!(
        h.delete(&format!("/jobs/{torn}")).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn half_built_upload_is_swept_on_restart() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    h.kill_server().await;
    let staging = h.data_dir().join("jobs").join(".staging-0000");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("program.gcode"), b"G1 X").unwrap();
    h.restart_server().await;
    h.wait_ready().await;
    assert!(!staging.exists());
    assert_eq!(h.get("/jobs").await.1, Value::Array(vec![]));
}

#[tokio::test]
async fn torn_position_file_does_not_stop_startup() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    h.kill_server().await;
    std::fs::write(h.data_dir().join("position.json"), b"\x00\x00garbage").unwrap();
    h.restart_server().await;
    h.wait_ready().await;
    let id = h.run(&zigzag(5)).await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn corrupt_settings_do_not_stop_the_server_or_jobs() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    h.kill_server().await;
    std::fs::write(h.data_dir().join("settings.json"), b"{not json").unwrap();
    h.restart_server().await;
    h.wait_ready().await;
    let id = h.run(&zigzag(5)).await;
    h.wait_job_state(&id, "completed").await;
}

// ---- Position record stays right after a lost acknowledgement -------------

#[tokio::test]
async fn position_is_still_recorded_after_a_lost_zero_sync_ack() {
    // A reconnection finds the controller somewhere else, so the sender
    // moves the work zeros to match. One of those lines is never
    // acknowledged; the sender gives up on it, and must not stop recording
    // where the tool is for the rest of its life.
    let mut h = Harness::start(SimConfig::grbl()).await;
    let first = h.run(&zigzag(5)).await;
    h.wait_job_state(&first, "completed").await;

    h.sim.disconnect();
    h.wait_disconnected().await;
    h.sim.set_position([5.0, 6.0, 7.0]);
    h.sim.mute_ack_of("G10 L2 P1");
    h.sim.reconnect(false).unwrap();
    wait_for("the zero sync to be sent", || async {
        h.sim
            .received_lines()
            .iter()
            .any(|l| l.contains("G10 L2 P1"))
            .then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let second = h.run(&zigzag(5)).await;
    h.wait_job_state(&second, "completed").await;
    h.kill_server().await;
    h.restart_server().await;
    h.wait_ready().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        h.machine().await["position_check"].is_null(),
        "the tool position was not recorded after the job moved it"
    );
}

// ---- Stale state carried from one job to the next -------------------------

#[tokio::test]
async fn new_job_after_an_alarm_is_not_confused_with_the_last_one() {
    // An alarm drops whatever the controller had buffered, so the lines that
    // were in flight are never acknowledged. Those must not be counted
    // against the next job.
    let h = Harness::start(slow()).await;
    let old = h.run(&zigzag(100)).await;
    h.wait_job(&old, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.trigger_alarm(1);
    h.wait_job_state(&old, "failed").await;
    h.wait_machine("the alarm", |m| m["alarm"] == 1 && m["ready"] == true)
        .await;

    let (status, body) = h.post("/machine/unlock").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_idle().await;

    let before = h.sim.received_lines().len();
    let fresh = h.run(&zigzag(5)).await;
    let job = h.wait_job_state(&fresh, "completed").await;
    assert_eq!(job["acked_lines"], 7);
    assert_eq!(job["total_lines"], 7);
    assert!(job["line_errors"].as_array().unwrap().is_empty());
    let sent: Vec<_> = h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect();
    assert_eq!(sent, zigzag_lines(5));
}

#[tokio::test]
async fn noise_without_a_line_end_does_not_swallow_the_next_reply() {
    // Junk that is not followed by a newline arrives right before real
    // replies, as it can after a board resets.
    let h = Harness::start(slow()).await;
    let id = h.run(&zigzag(60)).await;
    for _ in 0..40 {
        h.sim.inject(b"\xff\xfe");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 62);
    assert_eq!(program_lines(&h.sim), zigzag_lines(60));
}
