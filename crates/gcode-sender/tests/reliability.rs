//! Surviving dropped connections, restarts, resets, and bad serial data.

mod common;

use std::time::Duration;

use common::{Harness, program_lines, zigzag, zigzag_file_line, zigzag_lines};
use grbl_sim::SimConfig;
use reqwest::StatusCode;

/// Slow enough that the controller's planner is full while a job runs.
fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(20))
}

/// Planner depth of the simulated Grbl, plus the one line of margin.
const BACKUP: usize = 15 + 1;

async fn running_past(h: &Harness, moves: usize, acked: u64) -> String {
    let id = h.run(&zigzag(moves)).await;
    h.wait_job(&id, "enough progress", |j| {
        j["acked_lines"].as_u64() > Some(acked)
    })
    .await;
    id
}

/// What a resumed job must deliver: the modal state, then the program from
/// `index`.
fn resumed_stream(moves: usize, index: usize) -> Vec<String> {
    let mut lines = vec!["G21 G90 G17 G54 G94".to_string(), "F1000".to_string()];
    lines.extend_from_slice(&zigzag_lines(moves)[index..]);
    lines
}

#[tokio::test]
async fn lost_connection_interrupts_job_and_it_resumes_after_reconnect() {
    let h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 40).await;

    h.sim.disconnect();
    let job = h.wait_job_state(&id, "interrupted").await;
    assert!(job["reason"].as_str().unwrap().contains("connection"));
    let acked = job["acked_lines"].as_u64().unwrap() as usize;
    let m = h.wait_disconnected().await;
    assert_eq!(m["ready"], false);
    assert!(m["active_job"].is_null());

    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;
    let before = h.sim.received_lines().len();
    let index = acked - BACKUP;
    let job = h.action(&id, "resume").await;
    assert_eq!(job["state"], "running");
    assert_eq!(job["resumed_from_line"], zigzag_file_line(index));

    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 102);
    let sent: Vec<_> = h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect();
    assert_eq!(sent, resumed_stream(100, index));
    assert_eq!(h.sim.position(), [50.0, 1.0, 0.0]);
    assert!(!h.sim.overflowed());
}

#[tokio::test]
async fn controller_reset_loses_planned_moves_and_resume_replays_them() {
    // The LongBoard resets when the USB port reopens. Moves still in its
    // planner are lost, which is why a resume backs up past the planner.
    let h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 40).await;
    h.wait_machine("the planner to fill", |m| m["status"]["state"] == "Run")
        .await;

    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(true).unwrap();
    assert_eq!(h.sim.alarm(), Some(3), "reset in motion should alarm");
    let m = h
        .wait_machine("the alarm", |m| m["alarm"] == 3 && m["ready"] == true)
        .await;
    assert!(m["alarm_message"].as_str().unwrap().contains("reset"));

    // A resume would drive a machine whose position may be lost.
    let (status, body) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("alarm 3"));

    // The operator checks the machine, unlocks it, and carries on.
    let (status, _) = h.post("/machine/unlock").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_idle().await;
    assert!(program_lines(&h.sim).len() < 102);
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.sim.position(), [50.0, 1.0, 0.0]);

    // Every move was delivered at least once despite the lost planner.
    let sent = program_lines(&h.sim);
    for line in &zigzag_lines(100) {
        assert!(sent.contains(line), "{line} never reached the controller");
    }
}

#[tokio::test]
async fn server_restart_interrupts_job_and_it_resumes() {
    let mut h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 40).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    h.kill_server().await;
    h.restart_server().await;
    let job = h.job(&id).await;
    assert_eq!(job["state"], "interrupted");
    let acked = job["acked_lines"].as_u64().unwrap() as usize;
    assert!(acked > 17, "progress was not saved");

    h.wait_idle().await;
    let before = h.sim.received_lines().len();
    let job = h.action(&id, "resume").await;
    let index = acked - BACKUP;
    assert_eq!(job["resumed_from_line"], zigzag_file_line(index));
    h.wait_job_state(&id, "completed").await;
    let sent: Vec<_> = h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect();
    assert_eq!(sent, resumed_stream(100, index));
    assert_eq!(h.sim.position(), [50.0, 1.0, 0.0]);
}

#[tokio::test]
async fn resume_restores_units_spindle_and_feed() {
    let h = Harness::start(slow()).await;
    let mut program = String::from("G20 G90\nM3 S12000\nM8\nG1 F40\n");
    for i in 1..=60 {
        program.push_str(&format!("G1 X{}\n", i as f64 * 0.01));
    }
    let id = h.run(&program).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;

    let before = h.sim.received_lines().len();
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
    let sent: Vec<_> = h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect();
    assert_eq!(sent[..4], ["G20 G90 G17 G54 G94", "M8", "M3 S12000", "F40"]);
}

#[tokio::test]
async fn incremental_programs_cannot_be_resumed() {
    let h = Harness::start(slow()).await;
    let mut program = String::from("G91\n");
    for _ in 0..60 {
        program.push_str("G1 X0.1 F500\n");
    }
    let id = h.run(&program).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;

    let (status, body) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("G91"));
    // Starting over from the top is still allowed.
    h.action(&id, "start").await;
}

#[tokio::test]
async fn only_interrupted_jobs_can_be_resumed_without_an_active_job() {
    let h = Harness::start(slow()).await;
    let loaded = h.upload(&zigzag(5)).await;
    let (status, _) = h
        .post(&format!("/jobs/{}/resume", loaded["id"].as_str().unwrap()))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn resume_needs_a_connection() {
    let h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 20).await;
    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    let (status, _) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn silent_controller_is_treated_as_a_lost_connection() {
    let h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 20).await;

    h.sim.stall(true);
    let job = h.wait_job_state(&id, "interrupted").await;
    assert!(job["reason"].as_str().unwrap().contains("connection lost"));
    h.wait_disconnected().await;

    // When the controller comes back the server connects again by itself.
    h.sim.stall(false);
    h.wait_idle().await;
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn lost_acknowledgement_fails_the_job_and_recovers() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.drop_acks(1);
    let id = h.run(&zigzag(20)).await;

    let job = h.wait_job_state(&id, "failed").await;
    assert!(job["reason"].as_str().unwrap().contains("acknowledging"));
    // The server resets the controller so nothing unknown is left in it.
    h.wait_machine("a reset", |_| h.sim.soft_resets() == 1)
        .await;
    h.wait_idle().await;
    assert_eq!(h.sim.alarm(), None);

    let again = h.run(&zigzag(5)).await;
    h.wait_job_state(&again, "completed").await;
}

#[tokio::test]
async fn line_noise_is_ignored() {
    let h = Harness::start(slow()).await;
    let id = h.run(&zigzag(60)).await;
    for _ in 0..10 {
        h.sim.inject(b"\xff\xfe\x00garbage\r\n");
        h.sim.inject(b"<not a status\r\n");
        h.sim.inject(b"[BAD\r\nok?\r\n");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["acked_lines"], 62);
    assert_eq!(program_lines(&h.sim), zigzag_lines(60));
}

#[tokio::test]
async fn rejected_line_explains_the_error() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.error_on("X5.000", 33);
    let id = h.run(&zigzag(30)).await;
    let job = h.wait_job_state(&id, "paused").await;
    let error = &job["line_errors"][0];
    assert_eq!(error["code"], 33);
    assert_eq!(error["message"], "motion target is invalid");
    assert!(
        job["reason"]
            .as_str()
            .unwrap()
            .contains("motion target is invalid")
    );
}

#[tokio::test]
async fn alarm_is_explained_and_unlock_clears_it() {
    let h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 20).await;
    h.sim.trigger_alarm(1);
    let job = h.wait_job_state(&id, "failed").await;
    assert!(job["reason"].as_str().unwrap().contains("hard limit"));
    let m = h
        .wait_machine("the alarm", |m| m["alarm"] == 1 && m["ready"] == true)
        .await;
    assert!(m["alarm_message"].as_str().unwrap().contains("hard limit"));

    let next = h.upload(&zigzag(5)).await;
    let next = next["id"].as_str().unwrap();
    let (status, body) = h.post(&format!("/jobs/{next}/start")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("hard limit"));

    let (status, _) = h.post("/machine/unlock").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let m = h.wait_idle().await;
    assert!(m["alarm"].is_null());
    assert!(h.sim.received_lines().contains(&"$X".to_string()));
    h.action(next, "start").await;
    h.wait_job_state(next, "completed").await;
}

#[tokio::test]
async fn homing_runs_the_cycle() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_position([5.0, 6.0, 7.0]);
    h.wait_machine("the machine to be away from home", |m| {
        m["status"]["machine_position"] == serde_json::json!([5.0, 6.0, 7.0])
    })
    .await;
    let (status, _) = h.post("/machine/home").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_machine("the machine to be homed", |m| {
        m["status"]["machine_position"] == serde_json::json!([0.0, 0.0, 0.0])
    })
    .await;
    assert!(h.sim.received_lines().contains(&"$H".to_string()));
}

#[tokio::test]
async fn machine_actions_are_refused_during_a_job() {
    let h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 20).await;
    for action in ["unlock", "home", "reset"] {
        let (status, body) = h.post(&format!("/machine/{action}")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}: {body}");
    }
    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
}

#[tokio::test]
async fn reset_clears_a_hold_left_by_an_earlier_session() {
    let mut h = Harness::start(slow()).await;
    let id = running_past(&h, 100, 40).await;
    h.hold(&id).await;
    h.wait_machine("the hold", |m| m["status"]["substate"] == 0)
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    h.restart_server().await;
    let m = h
        .wait_machine("the hold", |m| m["status"]["state"] == "Hold")
        .await;
    assert!(m["active_job"].is_null());
    assert_eq!(h.job(&id).await["state"], "interrupted");

    // The machine is still holding, so the job cannot continue yet.
    let (status, body) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("Hold"));

    let (status, _) = h.post("/machine/reset").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_idle().await;
    assert_eq!(h.sim.alarm(), None, "resetting from a hold raised an alarm");
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.sim.position(), [50.0, 1.0, 0.0]);
}
