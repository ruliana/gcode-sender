//! Pausing lifts the tool and frees the machine; resuming brings the tool
//! back and lowers it.

mod common;

use std::time::Duration;

use common::{Harness, commands_sent, program_lines, wait_for};
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};

fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(20))
}

/// A cutting program: spindle on, down to Z-1, then `moves` small cuts.
fn cutting(moves: usize) -> String {
    let mut lines = vec![
        "G21 G90".to_string(),
        "M3 S12000".into(),
        "G0 Z5".into(),
        "G1 Z-1 F100".into(),
    ];
    for k in 1..=moves {
        lines.push(format!("G1 X{k} Y{} F600", k % 2));
    }
    lines.push("M5".into());
    lines.join("\n") + "\n"
}

async fn running(h: &Harness, moves: usize) -> String {
    let id = h.run(&cutting(moves)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    id
}

async fn pause(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    h.post_json(&format!("/jobs/{id}/pause"), &body).await
}

/// Waits for a pause to finish stopping the machine and lifting the tool.
async fn released(h: &Harness, id: &str) -> Value {
    let job = h
        .wait_job(id, "the pause to free the machine", |j| {
            j["state"] == "paused" && !j["paused_at"].is_null()
        })
        .await;
    h.wait_machine("the machine to be free", |m| {
        m["active_job"].is_null() && m["status"]["state"] == "Idle"
    })
    .await;
    job
}

fn jog_lines(h: &Harness) -> Vec<String> {
    h.sim
        .received_lines()
        .into_iter()
        .filter(|l| l.starts_with("$J="))
        .collect()
}

async fn set_settings(h: &Harness, body: Value) {
    let (status, reply) = h.put_json("/settings", &body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
}

#[tokio::test]
async fn pausing_lifts_the_tool_and_frees_the_machine() {
    let h = Harness::start(slow()).await;
    let id = running(&h, 300).await;

    let (status, job) = pause(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["state"], "paused");

    let job = released(&h, &id).await;
    let at = &job["paused_at"];
    let [x, y, _] = h.sim.position();
    // It stopped on the cut at Z-1, and went 10 mm up from there.
    assert_eq!(at["position"][2], -1.0);
    assert_eq!(at["lifted_to"], 9.0);
    assert!(at["position"][0].as_f64().unwrap() > 0.0);
    assert_eq!(h.sim.position(), [x, y, 9.0]);
    assert_eq!(
        json!([at["position"][0], at["position"][1]]),
        json!([x, y]),
        "the lift moved the tool sideways"
    );
    assert_eq!(jog_lines(&h), ["$J=G21 G90 G53 Z9.000 F2000.000"]);
    assert!(h.sim.realtime_bytes().contains(&b'!'));
    assert_eq!(h.sim.soft_resets(), 1);
    assert_eq!(job["reason"], "paused on request; the tool was lifted");

    // Nothing more is sent to the controller.
    let received = commands_sent(&h.sim).len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(commands_sent(&h.sim).len(), received);
}

#[tokio::test]
async fn the_tool_can_be_moved_while_paused() {
    let h = Harness::start(slow()).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;

    // Out of the way, for any reason.
    let (status, body) = h
        .post_json("/machine/jog", &json!({"x": 80, "y": 40, "feed": 3000}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_idle().await;
    assert_eq!(h.job(&id).await["state"], "paused");
}

#[tokio::test]
async fn resume_returns_to_the_work_and_lowers_the_tool() {
    let h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"dwell_s": 0.2}})).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    let job = released(&h, &id).await;
    let acked = job["acked_lines"].as_u64().unwrap();

    h.post_json("/machine/jog", &json!({"x": 80, "y": 40, "feed": 3000}))
        .await;
    h.wait_idle().await;
    let before = h.sim.received_lines().len();

    let (status, job) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["state"], "running");
    assert!(job["paused_at"].is_null());
    h.wait_job_state(&id, "completed").await;

    let sent: Vec<String> = h.sim.received_lines()[before..]
        .iter()
        .filter(|l| !l.starts_with('$'))
        .cloned()
        .collect();
    // Up, over, spindle back on, wait for it, plunge, then the program.
    assert_eq!(sent[0], "G21 G90 G94");
    assert_eq!(sent[1], "G53 G0 Z9.000");
    assert!(sent[2].starts_with("G53 G0 X"), "{sent:?}");
    assert_eq!(sent[3], "M3 S12000");
    assert_eq!(sent[4], "G4 P0.2");
    assert!(sent[5].starts_with("G53 G1 Z"), "{sent:?}");
    // The program carries on a few lines before where it stopped, since
    // the reset threw away what was queued.
    let restart = job["resumed_from_line"].as_u64().unwrap();
    assert!(restart <= acked, "restarted at {restart}, was at {acked}");
    assert_eq!(sent.last().unwrap(), "M5");
    assert_eq!(h.sim.position()[..2], [300.0, 0.0]);
}

#[tokio::test]
async fn resume_returns_even_if_the_tool_never_moved() {
    let h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"dwell_s": 0}})).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;
    let before = h.sim.received_lines().len();
    let (status, job) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    h.wait_job_state(&id, "completed").await;
    let sent: Vec<_> = h.sim.received_lines()[before..].to_vec();
    assert_eq!(sent[1], "G53 G0 Z9.000");
    assert!(
        sent.iter().all(|l| !l.starts_with("G4 ")),
        "dwell of zero must not be sent: {sent:?}"
    );
}

#[tokio::test]
async fn the_pause_behaviour_can_be_turned_off() {
    let h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"lift": false}})).await;
    let id = running(&h, 300).await;
    let (status, job) = pause(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    h.wait_machine("the hold to settle", |m| m["status"]["substate"] == 0)
        .await;

    // A plain hold: the job stays active, no reset, no lift.
    assert_eq!(h.machine().await["active_job"]["id"], id.as_str());
    assert_eq!(h.sim.soft_resets(), 0);
    assert!(jog_lines(&h).is_empty());
    assert!(job["paused_at"].is_null());

    let (status, job) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.sim.soft_resets(), 0);
}

#[tokio::test]
async fn one_pause_can_override_the_setting_either_way() {
    let h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"lift": false}})).await;
    let id = running(&h, 300).await;
    let (status, _) = pause(&h, &id, json!({"lift": true})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    released(&h, &id).await;
    assert_eq!(jog_lines(&h).len(), 1);
}

#[tokio::test]
async fn the_lift_can_be_chosen_per_pause() {
    let h = Harness::start(slow()).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({"height": 25})).await;
    let job = released(&h, &id).await;
    assert_eq!(job["paused_at"]["lifted_to"], 24.0);

    let h = Harness::start(slow()).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({"z": -0.25})).await;
    let job = released(&h, &id).await;
    assert_eq!(job["paused_at"]["lifted_to"], -0.25);
}

#[tokio::test]
async fn the_lift_stays_inside_the_z_boundary() {
    let h = Harness::start(slow()).await;
    let (status, _) = h.put_json("/machine/bounds", &json!({"z": [-20, 3]})).await;
    assert_eq!(status, StatusCode::OK);
    // The program goes up to Z5 first, so it never starts; use a program
    // that stays inside.
    let text = cutting(300).replace("G0 Z5", "G0 Z2");
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    pause(&h, &id, json!({})).await;
    let job = released(&h, &id).await;
    assert_eq!(job["paused_at"]["lifted_to"], 3.0);
    assert_eq!(jog_lines(&h), ["$J=G21 G90 G53 Z3.000 F2000.000"]);
}

#[tokio::test]
async fn a_tool_already_at_the_top_is_not_lifted() {
    let h = Harness::start(slow()).await;
    let (status, _) = h.put_json("/machine/bounds", &json!({"z": [-20, 0]})).await;
    assert_eq!(status, StatusCode::OK);
    // Cuts at the very top of the travel.
    let text = cutting(300)
        .replace("G0 Z5", "G0 Z0")
        .replace("G1 Z-1 F100", "G1 Z0 F100");
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    pause(&h, &id, json!({})).await;
    let job = released(&h, &id).await;
    assert!(job["paused_at"]["lifted_to"].is_null());
    assert!(
        job["reason"].as_str().unwrap().contains("not lifted"),
        "{job}"
    );
    assert!(jog_lines(&h).is_empty());
}

#[tokio::test]
async fn a_lift_the_controller_refuses_still_pauses_the_job() {
    let h = Harness::start(slow()).await;
    h.sim
        .set_travel([-500.0, -500.0, -50.0], [500.0, 500.0, 2.0]);
    let text = cutting(300).replace("G0 Z5", "G0 Z1");
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    pause(&h, &id, json!({})).await;
    let job = released(&h, &id).await;
    assert!(job["paused_at"]["lifted_to"].is_null());
    let reason = job["reason"].as_str().unwrap();
    assert!(
        reason.contains("refused the lift") && reason.contains("error 15"),
        "{reason}"
    );
    assert_eq!(h.machine().await["alarm"], Value::Null);

    // It can still be resumed, from where the tool is.
    set_settings(&h, json!({"pause": {"dwell_s": 0}})).await;
    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn a_job_that_cannot_be_picked_up_again_gets_a_plain_hold() {
    let h = Harness::start(slow()).await;
    let text = cutting(300).replacen("G21 G90", "G21 G90\nG92 X0", 1);
    let id = h.run(&text).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    let (status, job) = pause(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");
    assert_eq!(job["state"], "paused");
    let reason = job["reason"].as_str().unwrap();
    assert!(
        reason.contains("not lifted") && reason.contains("G92"),
        "{reason}"
    );
    h.wait_machine("the hold to settle", |m| m["status"]["substate"] == 0)
        .await;
    assert_eq!(h.sim.soft_resets(), 0);
    assert_eq!(h.machine().await["active_job"]["id"], id.as_str());

    h.action(&id, "resume").await;
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn bad_pause_options_are_refused_before_anything_happens() {
    let h = Harness::start(slow()).await;
    let id = running(&h, 300).await;
    for body in [
        json!({"height": 0}),
        json!({"height": "high"}),
        json!({"x": 1}),
    ] {
        let (status, reply) = pause(&h, &id, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {reply}");
    }
    assert_eq!(h.job(&id).await["state"], "running");
    assert!(!h.sim.realtime_bytes().contains(&b'!'));
}

#[tokio::test]
async fn a_pause_cannot_be_resumed_while_the_tool_is_still_lifting() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(1500))).await;
    let id = h.run(&cutting(20)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(8))
        .await;
    pause(&h, &id, json!({})).await;
    wait_for("the lift to be sent", || async {
        (!jog_lines(&h).is_empty()).then_some(())
    })
    .await;

    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("lifting"),
        "{body}"
    );

    released(&h, &id).await;
}

#[tokio::test]
async fn stopping_a_paused_job_ends_it_without_touching_the_machine() {
    let h = Harness::start(slow()).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;
    let resets = h.sim.soft_resets();
    let received = commands_sent(&h.sim).len();

    let job = h.action(&id, "stop").await;
    assert_eq!(job["state"], "stopped");
    assert!(job["paused_at"].is_null());
    assert_eq!(h.sim.soft_resets(), resets);
    assert_eq!(commands_sent(&h.sim).len(), received);
}

#[tokio::test]
async fn a_stop_during_the_lift_wins() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(1500))).await;
    let id = h.run(&cutting(20)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(8))
        .await;
    pause(&h, &id, json!({})).await;
    wait_for("the lift to be sent", || async {
        (!jog_lines(&h).is_empty()).then_some(())
    })
    .await;
    h.action(&id, "stop").await;
    let job = h.wait_job_state(&id, "stopped").await;
    assert!(job["paused_at"].is_null());
    h.wait_idle().await;
}

#[tokio::test]
async fn a_paused_job_survives_a_server_restart_and_can_be_resumed() {
    let mut h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"dwell_s": 0}})).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;

    h.restart_server().await;
    h.wait_ready().await;
    // The work offset is reported on a few of the status reports only.
    h.wait_machine("the work offset", |m| m["status"]["work_offset"].is_array())
        .await;
    let job = h.job(&id).await;
    assert_eq!(job["state"], "paused");
    assert!(!job["paused_at"].is_null());

    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn resuming_after_the_work_zero_moved_needs_to_be_accepted() {
    let h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"dwell_s": 0}})).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;

    let (status, body) = h.put_json("/machine/work-offset", &json!({"x": 5})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_machine("the new offset", |m| m["status"]["work_offset"][0] == 5.0)
        .await;
    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("work zero"));
    assert_eq!(h.job(&id).await["state"], "paused");

    let (status, body) = h
        .post_json(
            &format!("/jobs/{id}/resume"),
            &json!({"accept_moved_zero": true}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn settings_have_defaults_and_are_kept() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let (status, body) = h.get("/settings").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"pause": {"lift": true, "height": 10.0, "z": null, "dwell_s": 3.0}})
    );

    set_settings(
        &h,
        json!({"pause": {"lift": false, "height": 4, "z": -2.5}}),
    )
    .await;
    let expected = json!({"pause": {"lift": false, "height": 4.0, "z": -2.5, "dwell_s": 3.0}});
    assert_eq!(h.get("/settings").await.1, expected);

    h.restart_server().await;
    assert_eq!(h.get("/settings").await.1, expected);

    for bad in [
        json!({"pause": {"height": 0}}),
        json!({"pause": {"dwell_s": 120}}),
        json!({"pause": {"lift": "yes"}}),
        json!({"pause": {"speed": 1}}),
        json!({"jog": {}}),
    ] {
        let (status, _) = h.put_json("/settings", &bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    assert_eq!(h.get("/settings").await.1, expected);
}

#[tokio::test]
async fn program_lines_do_not_repeat_the_tool_commands_outside_the_return() {
    // Only the sequence that returns the tool carries the spindle command.
    let h = Harness::start(slow()).await;
    set_settings(&h, json!({"pause": {"dwell_s": 0}})).await;
    let id = running(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;
    h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    h.wait_job_state(&id, "completed").await;
    let spindle_on = program_lines(&h.sim)
        .iter()
        .filter(|l| l.starts_with("M3"))
        .count();
    assert_eq!(spindle_on, 2, "once at the start, once on the way back");
}

/// Sets the zero of `system` to machine coordinates `(x, y)`, and waits for
/// the controller to report it.
async fn set_zero(h: &Harness, system: &str, x: f64, y: f64) {
    let (status, body) = h
        .put_json(
            "/machine/work-offset",
            &json!({"x": x, "y": y, "system": system}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_machine(&format!("the {system} zero"), |m| {
        m["work"]["offsets"][system] == json!([x, y, 0.0])
    })
    .await;
}

/// The cutting program run in G55, with that zero at machine X100 Y50.
async fn running_in_g55(h: &Harness, moves: usize) -> String {
    set_zero(h, "G55", 100.0, 50.0).await;
    let id = h.run(&format!("G55\n{}", cutting(moves))).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(40))
        .await;
    id
}

#[tokio::test]
async fn a_pause_in_g55_lifts_and_resumes_in_its_own_zero() {
    let h = Harness::start(slow()).await;
    let id = running_in_g55(&h, 300).await;
    let (status, job) = pause(&h, &id, json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{job}");

    let job = released(&h, &id).await;
    // The pause point is in machine coordinates, so it is past the G55 zero.
    let at = &job["paused_at"];
    assert_eq!(at["position"][2], -1.0);
    assert_eq!(at["lifted_to"], 9.0);
    assert!(at["position"][0].as_f64().unwrap() > 100.0);

    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_job_state(&id, "completed").await;
    // X300 Y0 in G55 is X400 Y50 in machine coordinates.
    assert_eq!(h.sim.position(), [400.0, 50.0, -1.0]);
    assert!(
        h.sim
            .received_lines()
            .iter()
            .any(|l| l == "G21 G90 G17 G55 G94"),
        "resume did not restore G55"
    );
}

#[tokio::test]
async fn resuming_ignores_a_zero_the_program_does_not_use() {
    let h = Harness::start(slow()).await;
    let id = running_in_g55(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;

    set_zero(&h, "G54", 3.0, 0.0).await;
    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_job_state(&id, "completed").await;
}

#[tokio::test]
async fn resuming_refuses_a_moved_zero_the_program_uses_until_accepted() {
    let h = Harness::start(slow()).await;
    let id = running_in_g55(&h, 300).await;
    pause(&h, &id, json!({})).await;
    released(&h, &id).await;

    set_zero(&h, "G55", 110.0, 50.0).await;
    let (status, body) = h.post_json(&format!("/jobs/{id}/resume"), &json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("G55") && error.contains("accept_moved_zero"),
        "{error}"
    );
    assert_eq!(h.job(&id).await["state"], "paused");

    let (status, body) = h
        .post_json(
            &format!("/jobs/{id}/resume"),
            &json!({"accept_moved_zero": true}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    h.wait_job_state(&id, "completed").await;
    // X300 in G55 is now X410.
    assert_eq!(h.sim.position(), [410.0, 50.0, -1.0]);
}
