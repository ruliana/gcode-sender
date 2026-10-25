//! The tool's recorded position survives a restart, and a controller that
//! reports something else is corrected and asked about, not believed.

mod common;

use common::{Harness, zigzag};
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn near(v: &Value, want: [f64; 3]) -> bool {
    v.as_array().is_some_and(|a| {
        a.len() == 3
            && a.iter()
                .zip(want)
                .all(|(x, w)| (x.as_f64().unwrap() - w).abs() < 0.01)
    })
}

/// Records the tool at (10, 20, 5), then restarts the server with a
/// controller that reports (0, 0, 0): a reset, with the tool where it was.
async fn reset_while_the_tool_stayed_put(h: &mut Harness) {
    h.sim.set_position([10.0, 20.0, 5.0]);
    h.wait_machine("the position to be recorded", |m| {
        near(&m["status"]["machine_position"], [10.0, 20.0, 5.0])
    })
    .await;
    h.kill_server().await;
    h.sim.set_position([0.0, 0.0, 0.0]);
    h.restart_server().await;
    h.wait_ready().await;
}

#[tokio::test]
async fn a_reset_keeps_the_tool_where_it_was_recorded_and_asks() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    reset_while_the_tool_stayed_put(&mut h).await;

    // The work zero moved so the tool still reads where it was.
    let m = h
        .wait_machine("the work zero to be corrected", |m| {
            near(&m["status"]["work_position"], [10.0, 20.0, 5.0])
                && m["position_check"]["adjusted"] == true
        })
        .await;
    assert_eq!(h.sim.work_offset(), [-10.0, -20.0, -5.0]);
    assert!(
        m["position_check"]["question"]
            .as_str()
            .unwrap()
            .contains("moved")
    );
}

#[tokio::test]
async fn not_moved_keeps_the_correction_and_moved_undoes_it() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    reset_while_the_tool_stayed_put(&mut h).await;
    h.wait_machine("the correction", |m| {
        m["position_check"]["adjusted"] == true
    })
    .await;

    let (status, _) = h
        .post_json("/machine/position-check", &json!({ "moved": false }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_machine("the check to clear", |m| m["position_check"].is_null())
        .await;
    assert_eq!(h.sim.work_offset(), [-10.0, -20.0, -5.0]);

    // Nothing is waiting now, so another answer is refused.
    let (status, _) = h
        .post_json("/machine/position-check", &json!({ "moved": true }))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Reset again, and say the machine moved: the work zero goes back.
    h.kill_server().await;
    h.sim.set_position([3.0, 4.0, 1.0]);
    h.restart_server().await;
    h.wait_ready().await;
    h.wait_machine("the second check", |m| {
        m["position_check"]["adjusted"] == true
    })
    .await;
    let (status, _) = h
        .post_json("/machine/position-check", &json!({ "moved": true }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_machine("the check to clear", |m| m["position_check"].is_null())
        .await;
    assert_eq!(h.sim.work_offset(), [-10.0, -20.0, -5.0]);
}

#[tokio::test]
async fn without_trust_jobs_wait_for_the_answer() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let (status, _) = h
        .put_json(
            "/settings",
            &json!({ "position": { "trust_record": false } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    reset_while_the_tool_stayed_put(&mut h).await;
    h.wait_machine("the check to wait", |m| {
        m["position_check"]["adjusted"] == false
    })
    .await;

    // The work zero is untouched, and the job is refused.
    assert_eq!(h.sim.work_offset(), [0.0, 0.0, 0.0]);
    let id = h.upload(&zigzag(3)).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = h.post(&format!("/jobs/{id}/start")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, _) = h
        .post_json("/machine/position-check", &json!({ "moved": false }))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_machine("the check to clear", |m| m["position_check"].is_null())
        .await;
    assert_eq!(h.sim.work_offset(), [-10.0, -20.0, -5.0]);

    h.action(&id, "start").await;
    h.wait_job_state(&id, "completed").await;
}
