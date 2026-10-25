//! Optional machine boundaries, checked before a job sends anything.

mod common;

use common::{Harness, program_lines};
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

async fn set_bounds(h: &Harness, bounds: Value) {
    let (status, body) = h.put_json("/machine/bounds", &bounds).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Uploads `gcode` and tries to start it, returning the status and body.
async fn try_start(h: &Harness, gcode: &str) -> (String, StatusCode, Value) {
    let id = h.upload(gcode).await["id"].as_str().unwrap().to_string();
    let (status, body) = h.post(&format!("/jobs/{id}/start")).await;
    (id, status, body)
}

/// A program that must be refused, with nothing reaching the controller.
async fn assert_refused(h: &Harness, gcode: &str, expected: &[&str]) {
    let (id, status, body) = try_start(h, gcode).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap();
    for part in expected {
        assert!(error.contains(part), "{error:?} does not mention {part:?}");
    }
    assert_eq!(h.job(&id).await["state"], "loaded");
    assert!(program_lines(&h.sim).is_empty(), "lines were sent anyway");
}

#[tokio::test]
async fn boundaries_start_unset() {
    let h = Harness::start(SimConfig::grbl()).await;
    let (status, body) = h.get("/machine/bounds").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"x": null, "y": null, "z": null}));
}

#[tokio::test]
async fn boundaries_can_be_set_read_and_cleared() {
    let h = Harness::start(SimConfig::grbl()).await;
    let bounds = json!({"x": [0.0, 800.0], "y": [0.0, 400.0], "z": [-60.0, 0.0]});
    set_bounds(&h, bounds.clone()).await;
    assert_eq!(h.get("/machine/bounds").await.1, bounds);

    // Axes can be left out.
    set_bounds(&h, json!({"z": [-10, 0]})).await;
    assert_eq!(
        h.get("/machine/bounds").await.1,
        json!({"x": null, "y": null, "z": [-10.0, 0.0]})
    );

    assert_eq!(h.delete("/machine/bounds").await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        h.get("/machine/bounds").await.1,
        json!({"x": null, "y": null, "z": null})
    );
}

#[tokio::test]
async fn invalid_boundaries_are_rejected() {
    let h = Harness::start(SimConfig::grbl()).await;
    for bad in [
        json!({"x": [10, 0]}),
        json!({"x": [0]}),
        json!({"w": [0, 1]}),
        json!({"x": "wide"}),
    ] {
        let (status, _) = h.put_json("/machine/bounds", &bad).await;
        assert!(status.is_client_error(), "{bad} was accepted");
    }
    assert_eq!(
        h.get("/machine/bounds").await.1,
        json!({"x": null, "y": null, "z": null})
    );
}

#[tokio::test]
async fn boundaries_survive_a_restart() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"x": [0, 100]})).await;
    h.restart_server().await;
    assert_eq!(
        h.get("/machine/bounds").await.1,
        json!({"x": [0.0, 100.0], "y": null, "z": null})
    );
}

#[tokio::test]
async fn program_inside_the_boundaries_runs() {
    let h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"x": [0, 100], "y": [0, 100], "z": [-5, 0]})).await;
    let id = h
        .run("G21 G90\nG0 X0 Y0 Z0\nG1 Z-5 F100\nG1 X100 Y100\nG0 Z0\n")
        .await;
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.sim.position(), [100.0, 100.0, 0.0]);
}

#[tokio::test]
async fn program_outside_the_boundaries_is_refused_before_anything_is_sent() {
    let h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"x": [0, 100], "y": [0, 100], "z": [-5, 0]})).await;
    // The offending line is far into the program, after plenty of valid ones.
    let mut program = String::from("G21 G90\n");
    for i in 0..50 {
        program.push_str(&format!("G1 X{i} Y{i} F500\n"));
    }
    program.push_str("G1 X150 Y10\nG1 X0 Y0\n");
    assert_refused(
        &h,
        &program,
        &["line 52", "G1 X150 Y10", "X to 150", "0 to 100"],
    )
    .await;
    assert_refused(&h, "G90\nG1 Z-6 F100\n", &["line 2", "Z to -6"]).await;
    assert_refused(&h, "G90\nG1 Y-0.5 F100\n", &["Y to -0.500"]).await;
}

#[tokio::test]
async fn refused_job_can_be_started_after_the_boundaries_change() {
    let h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"x": [0, 10]})).await;
    let (id, status, _) = try_start(&h, "G90\nG1 X50 F500\n").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    set_bounds(&h, json!({"x": [0, 100]})).await;
    h.action(&id, "start").await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn boundaries_are_in_machine_coordinates() {
    // G54 zero sits at X100 Y50 in machine coordinates.
    let h = Harness::start(SimConfig::grbl()).await;
    h.sim.set_work_offset([100.0, 50.0, 0.0]);
    h.wait_machine("the work offset", |m| {
        m["status"]["work_position"] == json!([-100.0, -50.0, 0.0])
    })
    .await;
    set_bounds(&h, json!({"x": [0, 200], "y": [0, 100]})).await;

    // X50 in work coordinates is X150 in machine coordinates.
    let ok = h.run("G90\nG1 X50 Y20 F500\n").await;
    h.wait_job_state(&ok, "completed").await;
    assert_eq!(h.sim.position(), [150.0, 70.0, 0.0]);

    // X150 in work coordinates is X250 in machine coordinates.
    assert_refused_after(&h, "G90\nG1 X150 F500\n", &["X to 250"]).await;
    // G53 addresses machine coordinates directly.
    let ok = h.run("G53 G0 X10\n").await;
    h.wait_job_state(&ok, "completed").await;
    assert_refused_after(&h, "G53 G0 X250\n", &["X to 250"]).await;
}

/// Like [`assert_refused`] when earlier jobs already sent lines.
async fn assert_refused_after(h: &Harness, gcode: &str, expected: &[&str]) {
    let before = program_lines(&h.sim).len();
    let (id, status, body) = try_start(h, gcode).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap();
    for part in expected {
        assert!(error.contains(part), "{error:?} does not mention {part:?}");
    }
    assert_eq!(h.job(&id).await["state"], "loaded");
    assert_eq!(
        program_lines(&h.sim).len(),
        before,
        "lines were sent anyway"
    );
}

#[tokio::test]
async fn incremental_moves_are_followed_from_where_the_machine_is() {
    let h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"x": [0, 200]})).await;
    h.sim.set_position([190.0, 0.0, 0.0]);
    h.wait_machine("the position", |m| {
        m["status"]["machine_position"] == json!([190.0, 0.0, 0.0])
    })
    .await;
    assert_refused(&h, "G91\nG1 X20 F500\n", &["line 2", "X to 210"]).await;
    // Many small steps add up.
    let mut steps = String::from("G91\n");
    for _ in 0..30 {
        steps.push_str("G1 X-10 F500\n");
    }
    assert_refused(&h, &steps, &["line 21", "X to -10"]).await;
}

#[tokio::test]
async fn units_are_converted() {
    let h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"x": [0, 200]})).await;
    // 10 inches is 254 mm.
    assert_refused(&h, "G20 G90\nG1 X10 F20\n", &["X to 254"]).await;
    let ok = h.run("G20 G90\nG1 X7 F20\n").await;
    h.wait_job_state(&ok, "completed").await;
    assert_eq!(program_lines(&h.sim), ["G20 G90", "G1 X7 F20"]);
}

#[tokio::test]
async fn arcs_are_checked_along_their_whole_path() {
    let h = Harness::start(SimConfig::grbl()).await;
    // Both ends of this arc are inside the boundaries; the middle is not.
    set_bounds(&h, json!({"x": [-10, 20], "y": [-1, 6]})).await;
    let semicircle_up = "G90 G17\nG0 X0 Y0\nG2 X10 Y0 I5 J0 F500\n";
    let semicircle_down = "G90 G17\nG0 X0 Y0\nG3 X10 Y0 I5 J0 F500\n";
    // Clockwise from the left end passes over the top, reaching Y5.
    let ok = h.run(semicircle_up).await;
    h.wait_job_state(&ok, "completed").await;
    // Counter-clockwise passes under, reaching Y-5.
    assert_refused_after(&h, semicircle_down, &["line 3", "Y to -5"]).await;
    // The radius form describes the same arcs.
    assert_refused_after(&h, "G90 G17\nG0 X0 Y0\nG3 X10 Y0 R5 F500\n", &["Y to -5"]).await;
    let ok = h.run("G90 G17\nG0 X0 Y0\nG2 X10 Y0 R5 F500\n").await;
    h.wait_job_state(&ok, "completed").await;
    // A full circle reaches out on every side.
    assert_refused_after(
        &h,
        "G90 G17\nG0 X0 Y0\nG2 X0 Y0 I-8 J0 F500\n",
        &["X to -16"],
    )
    .await;
}

#[tokio::test]
async fn axes_without_boundaries_are_not_checked() {
    let h = Harness::start(SimConfig::grbl()).await;
    set_bounds(&h, json!({"z": [-5, 0]})).await;
    let ok = h.run("G90\nG1 X5000 Y-3000 F500\n").await;
    h.wait_job_state(&ok, "completed").await;
}

#[tokio::test]
async fn programs_that_cannot_be_followed_are_refused_only_when_boundaries_are_set() {
    let h = Harness::start(SimConfig::grbl()).await;
    let shifty = "G90\nG92 X0 Y0\nG1 X5 F500\n";
    let ok = h.run(shifty).await;
    h.wait_job_state(&ok, "completed").await;

    set_bounds(&h, json!({"x": [0, 100]})).await;
    let (id, status, body) = try_start(&h, shifty).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("G92") && error.contains("line 2"), "{error}");
    assert_eq!(h.job(&id).await["state"], "loaded");

    h.delete("/machine/bounds").await;
    h.action(&id, "start").await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn resuming_is_checked_against_the_boundaries_too() {
    let h = Harness::start(SimConfig::grbl().block_duration(std::time::Duration::from_millis(20)))
        .await;
    let mut program = String::from("G90\n");
    for i in 1..=80 {
        program.push_str(&format!("G1 X{i} F500\n"));
    }
    let id = h.run(&program).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    h.sim.disconnect();
    h.wait_job_state(&id, "interrupted").await;
    h.wait_disconnected().await;
    h.sim.reconnect(false).unwrap();
    h.wait_idle().await;

    // The operator tightens the boundaries while the job is interrupted.
    set_bounds(&h, json!({"x": [0, 60]})).await;
    let (status, body) = h.post(&format!("/jobs/{id}/resume")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"].as_str().unwrap().contains("X to 61"));
    assert_eq!(h.job(&id).await["state"], "interrupted");
}
