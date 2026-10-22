//! Setting the work zero (G54) and what depends on it: jogging in work
//! coordinates, boundaries, and resuming a job.

mod common;

use std::time::Duration;

use common::{Harness, wait_for, zigzag};
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn g10_lines(h: &Harness) -> Vec<String> {
    h.sim
        .received_lines()
        .into_iter()
        .filter(|l| l.contains("G10"))
        .collect()
}

/// Waits until `GET /machine` shows the work offset the controller has.
async fn wait_offset(h: &Harness, offset: [f64; 3]) -> Value {
    h.wait_machine(&format!("work offset {offset:?}"), |m| {
        m["status"]["work_offset"] == json!(offset)
    })
    .await
}

async fn accepted(h: &Harness, path: &str, body: Value) {
    let (status, reply) = h.post_json(path, &body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}: {reply}");
}

async fn set_offset(h: &Harness, body: Value) -> (StatusCode, Value) {
    h.put_json("/machine/work-offset", &body).await
}

/// A request that must be refused with nothing sent to the controller.
async fn assert_refused(h: &Harness, path: &str, body: Value, status: StatusCode, expected: &str) {
    let before = g10_lines(h).len();
    let (got, reply) = if path == "/machine/work-offset" {
        set_offset(h, body.clone()).await
    } else {
        h.post_json(path, &body).await
    };
    assert_eq!(got, status, "{body}: {reply}");
    let error = reply["error"].as_str().unwrap_or_default();
    assert!(error.contains(expected), "{error:?} lacks {expected:?}");
    assert_eq!(g10_lines(h).len(), before, "the change was sent anyway");
}

#[tokio::test]
async fn work_zero_makes_the_current_position_zero() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_position([30.0, 20.0, -5.0]);
    h.wait_machine("the position", |m| {
        m["status"]["machine_position"] == json!([30.0, 20.0, -5.0])
    })
    .await;

    accepted(&h, "/machine/work-zero", json!({"x": 0, "y": 0})).await;
    assert_eq!(g10_lines(&h), ["G21 G90 G10 L20 P1 X0.000 Y0.000"]);
    assert_eq!(h.sim.work_offset(), [30.0, 20.0, 0.0]);

    // The offset shows up as soon as the controller reports it, and the
    // work position follows.
    let m = wait_offset(&h, [30.0, 20.0, 0.0]).await;
    wait_for("the work position", || async {
        let m = h.machine().await;
        (m["status"]["work_position"] == json!([0.0, 0.0, -5.0])).then_some(())
    })
    .await;
    assert_eq!(m["last_error"], Value::Null);
}

#[tokio::test]
async fn work_zero_can_give_the_position_any_work_coordinate() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_position([30.0, 0.0, 12.0]);
    h.wait_machine("the position", |m| {
        m["status"]["machine_position"] == json!([30.0, 0.0, 12.0])
    })
    .await;
    // The surface is at Z12 and the stock is 10 mm thick.
    accepted(&h, "/machine/work-zero", json!({"z": 10, "x": -2.5})).await;
    assert_eq!(g10_lines(&h), ["G21 G90 G10 L20 P1 X-2.500 Z10.000"]);
    assert_eq!(h.sim.work_offset(), [32.5, 0.0, 2.0]);
}

#[tokio::test]
async fn work_offset_sets_where_work_zero_is_in_machine_coordinates() {
    let h = Harness::start(SimConfig::grbl()).await;
    let (status, body) = set_offset(&h, json!({"x": -100, "z": -20.5})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(g10_lines(&h), ["G21 G90 G10 L2 P1 X-100.000 Z-20.500"]);
    assert_eq!(h.sim.work_offset(), [-100.0, 0.0, -20.5]);
    wait_offset(&h, [-100.0, 0.0, -20.5]).await;
}

#[tokio::test]
async fn jogging_in_work_coordinates_follows_the_new_zero() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_position([30.0, 20.0, 0.0]);
    h.wait_machine("the position", |m| {
        m["status"]["machine_position"] == json!([30.0, 20.0, 0.0])
    })
    .await;
    accepted(&h, "/machine/work-zero", json!({"x": 0, "y": 0})).await;
    wait_offset(&h, [30.0, 20.0, 0.0]).await;

    accepted(&h, "/machine/jog", json!({"x": 15, "y": 5, "feed": 1000})).await;
    h.wait_idle().await;
    accepted(
        &h,
        "/machine/jog",
        json!({"x": 0, "y": 0, "feed": 1000, "mode": "absolute"}),
    )
    .await;
    h.wait_idle().await;
    wait_for("the tool to return to work zero", || async {
        (h.sim.position() == [30.0, 20.0, 0.0]).then_some(())
    })
    .await;
}

#[tokio::test]
async fn bad_requests_are_refused_before_reaching_the_controller() {
    let h = Harness::start(SimConfig::grbl()).await;
    let invalid = StatusCode::UNPROCESSABLE_ENTITY;
    for path in ["/machine/work-zero", "/machine/work-offset"] {
        assert_refused(&h, path, json!({}), invalid, "at least one").await;
        assert_refused(&h, path, json!({"w": 0}), invalid, "w").await;
        assert_refused(&h, path, json!({"x": "left"}), invalid, "x").await;
    }
    let (status, body) = h.post("/machine/work-zero").await;
    assert_eq!(status, invalid, "{body}");
    let (status, body) = h.put_json("/machine/work-offset", &Value::Null).await;
    assert_eq!(status, invalid, "{body}");
    assert!(g10_lines(&h).is_empty());
}

#[tokio::test]
async fn the_controllers_refusal_is_relayed_and_nothing_changes() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.error_on("G10", 3);
    let (status, body) = h.post_json("/machine/work-zero", &json!({"x": 0})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("error 3"),
        "{body}"
    );
    assert_eq!(h.sim.work_offset(), [0.0; 3]);
}

#[tokio::test]
async fn the_zero_cannot_change_during_a_job_an_alarm_or_a_move() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(400))).await;
    let conflict = StatusCode::CONFLICT;

    // Moving: the position is changing under the request.
    accepted(&h, "/machine/jog", json!({"x": 50, "feed": 1000})).await;
    h.wait_machine("the Jog state", |m| m["status"]["state"] == "Jog")
        .await;
    assert_refused(&h, "/machine/work-zero", json!({"x": 0}), conflict, "Jog").await;
    h.wait_idle().await;

    let id = h.run(&zigzag(20)).await;
    assert_refused(&h, "/machine/work-zero", json!({"x": 0}), conflict, &id).await;
    assert_refused(&h, "/machine/work-offset", json!({"x": 1}), conflict, &id).await;
    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
    h.wait_idle().await;

    h.sim.trigger_alarm(1);
    h.wait_machine("the alarm", |m| m["alarm"] == 1).await;
    assert_refused(&h, "/machine/work-zero", json!({"x": 0}), conflict, "alarm").await;
}

#[tokio::test]
async fn the_zero_cannot_change_while_disconnected() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.disconnect();
    h.wait_disconnected().await;
    assert_refused(
        &h,
        "/machine/work-zero",
        json!({"x": 0}),
        StatusCode::SERVICE_UNAVAILABLE,
        "not connected",
    )
    .await;
}

#[tokio::test]
async fn boundaries_follow_the_work_zero() {
    let h = Harness::start(SimConfig::grbl()).await;
    let (status, _) = h.put_json("/machine/bounds", &json!({"x": [0, 100]})).await;
    assert_eq!(status, StatusCode::OK);
    let id = h.upload("G21 G90\nG1 X10 F1000\n").await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // With the zero at machine X0, work X10 is inside the boundary.
    let (status, body) = set_offset(&h, json!({"x": 0})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_offset(&h, [0.0, 0.0, 0.0]).await;

    // With the zero at machine X-50, work X10 is machine X-40.
    let (status, body) = set_offset(&h, json!({"x": -50})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_offset(&h, [-50.0, 0.0, 0.0]).await;
    let (status, body) = h.post(&format!("/jobs/{id}/start")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("X") && error.contains("-40"), "{error}");
    assert_eq!(h.job(&id).await["state"], "loaded");

    set_offset(&h, json!({"x": 20})).await;
    wait_offset(&h, [20.0, 0.0, 0.0]).await;
    h.action(&id, "start").await;
    h.wait_job_state(&id, "completed").await;
}

/// A job that was stopped part of the way through.
async fn stopped_job(h: &Harness) -> String {
    let id = h.run(&zigzag(300)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
    h.wait_idle().await;
    id
}

#[tokio::test]
async fn a_job_remembers_the_zero_it_ran_with() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(20))).await;
    h.sim.set_work_offset([12.0, -3.0, 7.5]);
    wait_offset(&h, [12.0, -3.0, 7.5]).await;
    let id = h.run(&zigzag(5)).await;
    let job = h.wait_job_state(&id, "completed").await;
    assert_eq!(job["work_offset"], json!([12.0, -3.0, 7.5]));
}

#[tokio::test]
async fn resuming_after_the_zero_moved_is_refused_unless_accepted() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(20))).await;
    h.sim.set_work_offset([10.0, 10.0, 0.0]);
    wait_offset(&h, [10.0, 10.0, 0.0]).await;
    let id = stopped_job(&h).await;

    let (status, body) = set_offset(&h, json!({"x": 11})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_offset(&h, [11.0, 10.0, 0.0]).await;

    let before = h.sim.received_lines().len();
    let (status, body) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("work zero") && error.contains("accept_moved_zero"),
        "{error}"
    );
    assert_eq!(h.job(&id).await["state"], "stopped");
    assert_eq!(h.sim.received_lines().len(), before, "lines were sent");

    // Putting the zero back makes the job resumable again.
    set_offset(&h, json!({"x": 10})).await;
    wait_offset(&h, [10.0, 10.0, 0.0]).await;
    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn a_moved_zero_can_be_accepted_when_resuming() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(20))).await;
    let id = stopped_job(&h).await;
    set_offset(&h, json!({"y": 4.5})).await;
    wait_offset(&h, [0.0, 4.5, 0.0]).await;

    let (status, body) = h
        .post_json(
            &format!("/jobs/{id}/resume"),
            &json!({"accept_moved_zero": true}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let job = h.wait_job_state(&id, "completed").await;
    // The job now remembers the zero it finished with.
    assert_eq!(job["work_offset"], json!([0.0, 4.5, 0.0]));
}
