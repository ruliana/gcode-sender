//! Resuming jobs that ended early, with the options that make it safe.

mod common;

use std::time::Duration;

use common::{Harness, program_lines};
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(20))
}

/// Planner depth of the simulated Grbl, plus the one line of margin.
const BACKUP: usize = 15 + 1;

/// A cutting program: spindle on, down to Z-1, then `moves` small cuts.
/// Prepared line `3 + k` is cut `k`, which ends at X`k` Y`k % 2`. File line
/// numbers are the prepared index plus one, since it has no comments.
fn cutting(moves: usize) -> (String, Vec<String>) {
    let mut lines: Vec<String> = ["G21 G90", "M3 S12000", "G0 Z5", "G1 Z-1 F100"]
        .map(String::from)
        .into();
    for k in 1..=moves {
        lines.push(format!("G1 X{k} Y{}", k % 2));
    }
    lines.push("M5".into());
    let text = lines.join("\n") + "\n";
    (text, lines)
}

/// Runs a cutting program until well into the cuts, then unplugs the machine
/// and plugs it back in.
async fn interrupted_job(h: &Harness, moves: usize) -> (String, usize, Vec<String>) {
    let (text, lines) = cutting(moves);
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.disconnect();
    let job = h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;
    (id, job["acked_lines"].as_u64().unwrap() as usize, lines)
}

async fn resume(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    let res = reqwest::Client::new()
        .post(format!("{}/jobs/{id}/resume", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status();
    (status, res.json().await.unwrap_or(Value::Null))
}

fn sent_since(h: &Harness, before: usize) -> Vec<String> {
    h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect()
}

async fn assert_refused(h: &Harness, id: &str, body: Value, expected: &str) {
    let (status, reply) = resume(h, id, body.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {reply}");
    let error = reply["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(expected),
        "{error:?} does not mention {expected:?}"
    );
    assert_eq!(h.job(id).await["state"], "interrupted");
}

#[tokio::test]
async fn retract_lifts_travels_above_the_work_and_plunges_back() {
    let h = Harness::start(slow()).await;
    let (id, acked, lines) = interrupted_job(&h, 100).await;
    let index = acked - BACKUP;

    let before = h.sim.received_lines().len();
    let (status, job) = resume(&h, &id, json!({"retract": {"z": 10, "dwell_s": 0.5}})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    h.wait_job_state(&id, "completed").await;

    // The tool was at the end of the line before the resume point.
    let cut = index - 4;
    let mut expected = vec![
        "G21 G90 G94".to_string(),
        "G53 G0 Z10.000".to_string(),
        format!("G53 G0 X{cut}.000 Y{}.000", cut % 2),
        "M3 S12000".to_string(),
        "G4 P0.5".to_string(),
        "G53 G1 Z-1.000 F100.000".to_string(),
        "G21 G90 G17 G54 G94".to_string(),
        "F100".to_string(),
    ];
    expected.extend_from_slice(&lines[index..]);
    assert_eq!(sent_since(&h, before), expected);
    assert_eq!(h.sim.position(), [100.0, 0.0, -1.0]);
    assert!(!h.sim.overflowed());
}

#[tokio::test]
async fn retract_plunges_at_the_requested_feed() {
    let h = Harness::start(slow()).await;
    let (id, _, _) = interrupted_job(&h, 100).await;
    let before = h.sim.received_lines().len();
    let (status, _) = resume(&h, &id, json!({"retract": {"z": 3, "plunge_feed": 25}})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_job_state(&id, "completed").await;
    let sent = sent_since(&h, before);
    assert!(
        sent.contains(&"G53 G1 Z-1.000 F25.000".to_string()),
        "{sent:?}"
    );
    // Without a dwell there is no wait.
    assert!(!sent.iter().any(|l| l.starts_with("G4")));
}

#[tokio::test]
async fn a_retract_that_would_go_down_is_refused() {
    let h = Harness::start(slow()).await;
    let (id, _, _) = interrupted_job(&h, 100).await;

    // The tool is at Z-1 and the resume point is at Z-1.
    assert_refused(&h, &id, json!({"retract": {"z": -4}}), "has to go up").await;

    // Above the resume point but below where the tool is now.
    h.sim.set_position([20.0, 0.0, 3.0]);
    h.wait_machine("the new position", |m| {
        m["status"]["machine_position"][2] == 3.0
    })
    .await;
    assert_refused(&h, &id, json!({"retract": {"z": 2}}), "current Z 3.000").await;

    // Below the resume point, though above the tool.
    h.sim.set_position([20.0, 0.0, -3.0]);
    h.wait_machine("the new position", |m| {
        m["status"]["machine_position"][2] == -3.0
    })
    .await;
    assert_refused(
        &h,
        &id,
        json!({"retract": {"z": -2}}),
        "resume point's Z -1.000",
    )
    .await;
}

#[tokio::test]
async fn a_retract_outside_the_boundaries_is_refused() {
    let h = Harness::start(slow()).await;
    let (id, _, _) = interrupted_job(&h, 100).await;
    let (status, _) = h.put_json("/machine/bounds", &json!({"z": [-5, 5]})).await;
    assert_eq!(status, StatusCode::OK);
    assert_refused(&h, &id, json!({"retract": {"z": 10}}), "Z boundary").await;
    let (status, _) = resume(&h, &id, json!({"retract": {"z": 5}})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn a_retract_needs_a_feed_rate_to_plunge_with() {
    let h = Harness::start(slow()).await;
    // This program never sets a feed rate.
    let mut text = String::from("G21 G90\nM3 S12000\n");
    for k in 1..=100 {
        text.push_str(&format!("G1 X{k}\n"));
    }
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;

    assert_refused(&h, &id, json!({"retract": {"z": 5}}), "plunge_feed").await;
    let (status, _) = resume(&h, &id, json!({"retract": {"z": 5, "plunge_feed": 40}})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn nonsense_options_are_refused() {
    let h = Harness::start(slow()).await;
    let (id, _, _) = interrupted_job(&h, 100).await;
    assert_refused(
        &h,
        &id,
        json!({"retract": {"z": 5, "dwell_s": 100}}),
        "dwell_s",
    )
    .await;
    assert_refused(
        &h,
        &id,
        json!({"retract": {"z": 5, "plunge_feed": -1}}),
        "plunge_feed",
    )
    .await;
    assert_refused(&h, &id, json!({"retract": {"z": 5, "speed": 3}}), "speed").await;
    assert_refused(&h, &id, json!({"retract": {}}), "z").await;
    assert_refused(&h, &id, json!({"back_up": 3}), "back_up").await;
    assert_refused(&h, &id, json!({"from_line": 100000}), "past the end").await;
}

#[tokio::test]
async fn resume_can_start_from_a_chosen_line() {
    let h = Harness::start(slow()).await;
    let (id, _, lines) = interrupted_job(&h, 100).await;

    // File line 21 is prepared line 20; no backing up happens.
    let before = h.sim.received_lines().len();
    let (status, job) = resume(&h, &id, json!({"from_line": 21})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["resumed_from_line"], 21);
    h.wait_job_state(&id, "completed").await;

    let mut expected = vec![
        "G21 G90 G17 G54 G94".to_string(),
        "M3 S12000".to_string(),
        "F100".to_string(),
    ];
    expected.extend_from_slice(&lines[20..]);
    assert_eq!(sent_since(&h, before), expected);
    assert_eq!(h.sim.position(), [100.0, 0.0, -1.0]);
}

#[tokio::test]
async fn a_chosen_line_that_is_not_code_means_the_next_code() {
    let h = Harness::start(slow()).await;
    let mut text = String::from("; header\nG21 G90\n(a comment)\n\nG1 X1 F100\n");
    for k in 2..=100 {
        text.push_str(&format!("G1 X{k}\n"));
    }
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;

    // Lines 3 and 4 are a comment and a blank line; line 5 is code.
    let (status, job) = resume(&h, &id, json!({"from_line": 3})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["resumed_from_line"], 5);
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn resume_can_restart_from_the_top() {
    let h = Harness::start(slow()).await;
    let (id, _, _) = interrupted_job(&h, 100).await;
    let (status, job) = resume(&h, &id, json!({"from_line": 0})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["resumed_from_line"], 1);
    assert_eq!(job["acked_lines"], 0);
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn a_stopped_job_can_be_resumed() {
    let h = Harness::start(slow()).await;
    let (text, lines) = cutting(100);
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.action(&id, "stop").await;
    let job = h.wait_job_state(&id, "stopped").await;
    let acked = job["acked_lines"].as_u64().unwrap() as usize;
    h.wait_idle().await;
    assert_eq!(h.sim.alarm(), None);

    // The reset threw away what was in the planner, so the job backs up.
    let before = h.sim.received_lines().len();
    let (status, job) = resume(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["resumed_from_line"], acked - BACKUP + 1);
    h.wait_job_state(&id, "completed").await;
    let sent = sent_since(&h, before);
    assert_eq!(
        sent[sent.len() - (lines.len() - (acked - BACKUP))..],
        lines[acked - BACKUP..]
    );
    assert_eq!(h.sim.position(), [100.0, 0.0, -1.0]);
}

#[tokio::test]
async fn a_job_failed_by_an_alarm_can_be_resumed_after_unlocking() {
    let h = Harness::start(slow()).await;
    let (text, _) = cutting(100);
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.trigger_alarm(1);
    h.wait_job_state(&id, "failed").await;
    h.wait_machine("the alarm", |m| m["alarm"] == 1 && m["ready"] == true)
        .await;

    // The operator has to deal with the alarm first.
    let (status, reply) = resume(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply}");
    assert!(reply["error"].as_str().unwrap().contains("alarm 1"));

    h.post("/machine/unlock").await;
    h.wait_idle().await;
    let (status, reply) = resume(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn options_do_not_apply_to_a_paused_job() {
    let h = Harness::start(slow()).await;
    let (text, _) = cutting(100);
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(20))
        .await;
    h.hold(&id).await;
    let (status, reply) = resume(&h, &id, json!({"from_line": 10})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reply}");
    assert_eq!(h.job(&id).await["state"], "paused");
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
    assert!(program_lines(&h.sim).len() > 100);
}

#[tokio::test]
async fn completed_and_loaded_jobs_cannot_be_resumed() {
    let h = Harness::start(slow()).await;
    let (text, _) = cutting(5);
    let loaded = h.upload(&text).await["id"].as_str().unwrap().to_string();
    let (status, _) = resume(&h, &loaded, json!({"from_line": 2})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let id = h.run(&text).await;
    h.wait_job_state(&id, "completed").await;
    let (status, _) = resume(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
