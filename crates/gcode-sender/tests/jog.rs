//! Jogging: moving the tool by hand through `POST /machine/jog`.

mod common;

use std::time::Duration;

use common::{Harness, TIMEOUT, wait_for, zigzag};
use grbl_sim::{SimConfig, Simulator};
use reqwest::StatusCode;
use serde_json::{Value, json};

/// A controller whose moves take long enough to watch and interrupt.
fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(600))
}

fn jog_lines(sim: &Simulator) -> Vec<String> {
    sim.received_lines()
        .into_iter()
        .filter(|l| l.starts_with("$J="))
        .collect()
}

async fn jog(h: &Harness, body: Value) -> (StatusCode, Value) {
    let (status, reply) = h.post_json("/machine/jog", &body).await;
    (status, reply)
}

async fn assert_jogs(h: &Harness, body: Value) {
    let (status, reply) = jog(h, body.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}: {reply}");
}

/// A jog that must be refused with nothing reaching the controller.
async fn assert_refused(h: &Harness, body: Value, status: StatusCode, expected: &[&str]) {
    let before = jog_lines(&h.sim).len();
    let (got, reply) = jog(h, body.clone()).await;
    assert_eq!(got, status, "{body}: {reply}");
    let error = reply["error"].as_str().unwrap();
    for part in expected {
        assert!(error.contains(part), "{error:?} does not mention {part:?}");
    }
    assert_eq!(jog_lines(&h.sim).len(), before, "the jog was sent anyway");
}

async fn wait_position(h: &Harness, expected: [f64; 3]) {
    wait_for(&format!("the tool to reach {expected:?}"), || async {
        let p = h.sim.position();
        p.iter()
            .zip(expected)
            .all(|(a, b)| (a - b).abs() < 1e-6)
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_relative_jog_moves_the_tool_and_reports_the_jog_state() {
    let h = Harness::start(slow()).await;
    assert_jogs(&h, json!({"x": 10, "y": -2.5, "feed": 1500})).await;
    assert_eq!(jog_lines(&h.sim), ["$J=G21 G91 X10.000 Y-2.500 F1500.000"]);

    let m = h
        .wait_machine("the Jog state", |m| m["status"]["state"] == "Jog")
        .await;
    assert_eq!(m["alarm"], Value::Null);
    h.wait_idle().await;
    wait_position(&h, [10.0, -2.5, 0.0]).await;

    // A second jog is relative to where the tool now is.
    assert_jogs(&h, json!({"x": 5, "feed": 1500})).await;
    h.wait_idle().await;
    wait_position(&h, [15.0, -2.5, 0.0]).await;
}

#[tokio::test]
async fn jogs_sent_while_jogging_queue_up() {
    let h = Harness::start(slow()).await;
    assert_jogs(&h, json!({"x": 10, "feed": 1000})).await;
    assert_jogs(&h, json!({"x": 10, "feed": 1000})).await;
    assert_jogs(&h, json!({"z": 3, "feed": 1000})).await;
    assert_eq!(jog_lines(&h.sim).len(), 3);
    h.wait_idle().await;
    wait_position(&h, [20.0, 0.0, 3.0]).await;
}

#[tokio::test]
async fn an_absolute_jog_in_machine_coordinates_ignores_the_work_offset() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_work_offset([-100.0, 0.0, 0.0]);
    assert_jogs(
        &h,
        json!({"x": 40, "y": 5, "feed": 800, "mode": "absolute", "space": "machine"}),
    )
    .await;
    assert_eq!(
        jog_lines(&h.sim),
        ["$J=G21 G90 G53 X40.000 Y5.000 F800.000"]
    );
    h.wait_idle().await;
    wait_position(&h, [40.0, 5.0, 0.0]).await;
}

#[tokio::test]
async fn an_absolute_jog_defaults_to_work_coordinates() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_work_offset([-100.0, 0.0, 0.0]);
    assert_jogs(&h, json!({"x": 0, "feed": 800, "mode": "absolute"})).await;
    assert_eq!(jog_lines(&h.sim), ["$J=G21 G90 X0.000 F800.000"]);
    h.wait_idle().await;
    // Work zero is at machine X-100.
    wait_position(&h, [-100.0, 0.0, 0.0]).await;
}

#[tokio::test]
async fn cancelling_stops_the_jog_where_it_is() {
    let h = Harness::start(slow()).await;
    assert_jogs(&h, json!({"x": 200, "feed": 1000})).await;
    assert_jogs(&h, json!({"x": 200, "feed": 1000})).await;
    h.wait_machine("the Jog state", |m| m["status"]["state"] == "Jog")
        .await;

    let (status, body) = h.post("/machine/jog/cancel").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    wait_for("the jog cancel byte", || async {
        h.sim.realtime_bytes().contains(&0x85).then_some(())
    })
    .await;

    h.wait_idle().await;
    // Both jogs were dropped; the simulator completes no block of either.
    let [x, ..] = h.sim.position();
    assert!(x < 200.0, "the jog ran to the end: x = {x}");
    assert_eq!(h.sim.soft_resets(), 0, "a jog cancel must not reset");
    assert_eq!(h.machine().await["alarm"], Value::Null);

    // The machine is usable at once.
    assert_jogs(&h, json!({"y": 1, "feed": 1000})).await;
}

#[tokio::test]
async fn cancelling_with_no_jog_is_harmless() {
    let h = Harness::start(SimConfig::grbl()).await;
    let (status, _) = h.post("/machine/jog/cancel").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.wait_idle().await;
}

#[tokio::test]
async fn malformed_jogs_are_refused_before_reaching_the_controller() {
    let h = Harness::start(SimConfig::grbl()).await;
    let invalid = StatusCode::UNPROCESSABLE_ENTITY;
    assert_refused(&h, json!({"feed": 100}), invalid, &["at least one"]).await;
    assert_refused(&h, json!({"x": 1, "feed": 0}), invalid, &["feed"]).await;
    assert_refused(&h, json!({"x": 1, "feed": -5}), invalid, &["feed"]).await;
    assert_refused(&h, json!({"x": 1, "feed": 100, "w": 3}), invalid, &["w"]).await;
    assert_refused(&h, json!({"x": "far", "feed": 100}), invalid, &["x"]).await;
    assert_refused(
        &h,
        json!({"x": 1, "feed": 100, "mode": "sideways"}),
        invalid,
        &["sideways"],
    )
    .await;
    assert_refused(
        &h,
        json!({"x": 1, "feed": 100, "space": "machine"}),
        invalid,
        &["absolute"],
    )
    .await;

    // A missing body is refused too.
    let (status, body) = h.post("/machine/jog").await;
    assert_eq!(status, invalid, "{body}");
    assert!(jog_lines(&h.sim).is_empty());
}

#[tokio::test]
async fn the_controllers_own_refusal_is_relayed() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim
        .set_travel([-100.0, -100.0, -50.0], [100.0, 100.0, 0.0]);
    let (status, body) = jog(&h, json!({"x": 500, "feed": 1000})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("error 15") && error.contains("travel"),
        "{error}"
    );
    assert_eq!(jog_lines(&h.sim).len(), 1);
    // Nothing moved, and the machine is not left in an error state.
    assert_eq!(h.sim.position(), [0.0, 0.0, 0.0]);
    h.wait_idle().await;
    assert_jogs(&h, json!({"x": 5, "feed": 1000})).await;
}

#[tokio::test]
async fn boundaries_stop_a_jog_that_would_leave_them() {
    let h = Harness::start(slow()).await;
    let (status, _) = h
        .put_json("/machine/bounds", &json!({"x": [0, 100], "z": [-20, 0]}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let invalid = StatusCode::UNPROCESSABLE_ENTITY;

    assert_refused(
        &h,
        json!({"x": 150, "feed": 1000}),
        invalid,
        &["X", "boundary"],
    )
    .await;
    assert_refused(
        &h,
        json!({"x": -1, "feed": 1000}),
        invalid,
        &["X", "boundary"],
    )
    .await;
    assert_refused(
        &h,
        json!({"z": 1, "feed": 1000}),
        invalid,
        &["Z", "boundary"],
    )
    .await;
    assert_refused(
        &h,
        json!({"x": 101, "feed": 1000, "mode": "absolute", "space": "machine"}),
        invalid,
        &["X", "boundary"],
    )
    .await;
    // Y has no boundary.
    assert_jogs(&h, json!({"y": 500, "feed": 1000})).await;
    h.wait_idle().await;

    // Edges are inside.
    assert_jogs(&h, json!({"x": 100, "feed": 1000})).await;
}

#[tokio::test]
async fn boundaries_account_for_jogs_still_queued() {
    let h = Harness::start(slow()).await;
    let (status, _) = h.put_json("/machine/bounds", &json!({"x": [0, 100]})).await;
    assert_eq!(status, StatusCode::OK);

    assert_jogs(&h, json!({"x": 60, "feed": 1000})).await;
    // The tool has not moved yet, but it will be at 60.
    assert_refused(
        &h,
        json!({"x": 60, "feed": 1000}),
        StatusCode::UNPROCESSABLE_ENTITY,
        &["X", "120"],
    )
    .await;
    assert_jogs(&h, json!({"x": 40, "feed": 1000})).await;
    h.wait_idle().await;
    wait_position(&h, [100.0, 0.0, 0.0]).await;
}

#[tokio::test]
async fn boundaries_apply_to_work_coordinates_through_the_offset() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_work_offset([-50.0, 0.0, 0.0]);
    let (status, _) = h.put_json("/machine/bounds", &json!({"x": [-50, 0]})).await;
    assert_eq!(status, StatusCode::OK);
    // The offset arrives on a periodic report; give it time.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Work X 60 is machine X 10: outside [-50, 0].
    assert_refused(
        &h,
        json!({"x": 60, "feed": 1000, "mode": "absolute"}),
        StatusCode::UNPROCESSABLE_ENTITY,
        &["X", "10.000"],
    )
    .await;
    // Work X 20 is machine X -30.
    assert_jogs(&h, json!({"x": 20, "feed": 1000, "mode": "absolute"})).await;
    h.wait_idle().await;
    wait_position(&h, [-30.0, 0.0, 0.0]).await;
}

#[tokio::test]
async fn no_jogging_during_a_job() {
    let h = Harness::start(slow()).await;
    let id = h.run(&zigzag(20)).await;
    assert_refused(
        &h,
        json!({"x": 1, "feed": 100}),
        StatusCode::CONFLICT,
        &[&id],
    )
    .await;
    let (status, body) = h.post("/machine/jog/cancel").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!h.sim.realtime_bytes().contains(&0x85));

    h.action(&id, "stop").await;
    h.wait_job_state(&id, "stopped").await;
    h.wait_idle().await;
    assert_jogs(&h, json!({"x": 1, "feed": 100})).await;
}

#[tokio::test]
async fn no_jogging_during_an_alarm() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.trigger_alarm(1);
    h.wait_machine("the alarm", |m| m["alarm"] == 1).await;
    assert_refused(
        &h,
        json!({"x": 1, "feed": 100}),
        StatusCode::CONFLICT,
        &["alarm"],
    )
    .await;
}

#[tokio::test]
async fn no_jogging_while_disconnected() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.disconnect();
    h.wait_disconnected().await;
    assert_refused(
        &h,
        json!({"x": 1, "feed": 100}),
        StatusCode::SERVICE_UNAVAILABLE,
        &["not connected"],
    )
    .await;
}

#[tokio::test]
async fn jogging_works_on_grblhal() {
    let h = Harness::start(SimConfig::grblhal()).await;
    assert_jogs(&h, json!({"x": 3, "z": -1, "feed": 600})).await;
    h.wait_idle().await;
    wait_position(&h, [3.0, 0.0, -1.0]).await;
}

#[tokio::test]
async fn a_jog_in_flight_when_the_link_drops_is_not_reported_as_done() {
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.drop_acks(1);
    let (status, body) = tokio::time::timeout(TIMEOUT, jog(&h, json!({"x": 1, "feed": 100})))
        .await
        .expect("the request must not hang");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

async fn put_settings(h: &Harness, body: Value) {
    let (status, reply) = h.put_json("/settings", &body).await;
    assert_eq!(status, StatusCode::OK, "{body}: {reply}");
}

#[tokio::test]
async fn a_jog_without_a_feed_uses_the_saved_speed_for_its_axes() {
    let h = Harness::start(SimConfig::grbl()).await;

    assert_jogs(&h, json!({"x": 1})).await;
    h.wait_idle().await;
    assert_jogs(&h, json!({"z": 1})).await;
    h.wait_idle().await;
    assert_jogs(&h, json!({"x": 1, "y": 1})).await;
    h.wait_idle().await;
    assert_jogs(&h, json!({"x": 1, "z": 1})).await;
    h.wait_idle().await;
    assert_eq!(
        jog_lines(&h.sim),
        [
            "$J=G21 G91 X1.000 F1000.000",
            "$J=G21 G91 Z1.000 F300.000",
            "$J=G21 G91 X1.000 Y1.000 F1000.000",
            "$J=G21 G91 X1.000 Z1.000 F300.000",
        ]
    );

    // XY and Z have their own speeds, saved with the other settings.
    put_settings(&h, json!({"jog": {"xy_feed": 800, "z_feed": 150}})).await;
    assert_eq!(
        h.get("/settings").await.1["jog"],
        json!({"xy_feed": 800.0, "z_feed": 150.0})
    );
    assert_jogs(&h, json!({"y": 1})).await;
    h.wait_idle().await;
    assert_jogs(&h, json!({"z": -1})).await;
    h.wait_idle().await;
    assert_jogs(&h, json!({"x": 1, "y": 1, "z": 1})).await;
    h.wait_idle().await;
    let lines = jog_lines(&h.sim);
    assert_eq!(lines[4], "$J=G21 G91 Y1.000 F800.000");
    assert_eq!(lines[5], "$J=G21 G91 Z-1.000 F150.000");
    assert_eq!(lines[6], "$J=G21 G91 X1.000 Y1.000 Z1.000 F150.000");
}

#[tokio::test]
async fn an_explicit_feed_still_wins_over_the_saved_speeds() {
    let h = Harness::start(SimConfig::grbl()).await;
    put_settings(&h, json!({"jog": {"xy_feed": 800, "z_feed": 150}})).await;
    assert_jogs(&h, json!({"x": 2, "feed": 2500})).await;
    h.wait_idle().await;
    assert_eq!(jog_lines(&h.sim), ["$J=G21 G91 X2.000 F2500.000"]);
}

#[tokio::test]
async fn jog_speeds_are_validated_and_kept_across_restarts() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let expected = json!({"xy_feed": 1000.0, "z_feed": 300.0});
    assert_eq!(h.get("/settings").await.1["jog"], expected);

    put_settings(&h, json!({"jog": {"z_feed": 120}})).await;
    let expected = json!({"xy_feed": 1000.0, "z_feed": 120.0});
    h.restart_server().await;
    assert_eq!(h.get("/settings").await.1["jog"], expected);

    let invalid = StatusCode::UNPROCESSABLE_ENTITY;
    for bad in [
        json!({"jog": {"xy_feed": 0}}),
        json!({"jog": {"z_feed": -10}}),
        json!({"jog": {"z_feed": 20000}}),
        json!({"jog": {"speed": 1}}),
    ] {
        let (status, _) = h.put_json("/settings", &bad).await;
        assert_eq!(status, invalid, "{bad}");
    }
    assert_eq!(h.get("/settings").await.1["jog"], expected);
}
