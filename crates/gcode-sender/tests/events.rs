//! The live event stream, `GET /events`.

mod common;

use std::time::Duration;

use common::{Harness, zigzag};
use grbl_sim::SimConfig;
use serde_json::{Value, json};

fn slow() -> SimConfig {
    SimConfig::grbl().block_duration(Duration::from_millis(30))
}

#[tokio::test]
async fn the_stream_starts_with_the_current_state() {
    let h = Harness::start(SimConfig::grbl()).await;
    let mut events = h.events().await;
    assert!(events.content_type().starts_with("text/event-stream"));

    let (name, machine) = events.next().await;
    assert_eq!(name, "machine");
    assert_eq!(machine["connection"], "connected");
    assert_eq!(machine["ready"], true);
    assert_eq!(machine["status"]["state"], "Idle");
    // It is what GET /machine says, so a client can use either.
    assert_eq!(machine["port"], h.machine().await["port"]);
}

#[tokio::test]
async fn an_idle_machine_sends_nothing_new() {
    let h = Harness::start(SimConfig::grbl()).await;
    let mut events = h.events().await;
    events.next().await;
    // The controller reports every 50 ms, but nothing changed.
    assert!(
        events
            .next_within(Duration::from_millis(800))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_job_is_followed_from_start_to_finish() {
    let h = Harness::start(slow()).await;
    let mut events = h.events().await;
    events.next().await;

    let id = h.run(&zigzag(40)).await;
    let running = events
        .wait_for("job", "the job to be running", |j| {
            j["id"] == id.as_str() && j["state"] == "running"
        })
        .await;
    assert_eq!(running["total_lines"], 42);

    // Progress arrives while it runs, in both event kinds.
    let machine = events
        .wait_for("machine", "the machine to run the job", |m| {
            m["status"]["state"] == "Run" && m["active_job"]["id"] == id.as_str()
        })
        .await;
    assert!(machine["status"]["machine_position"].is_array());

    // The last job event is the final record, after the machine has let go.
    let done = events
        .wait_for("job", "the job to complete", |j| j["state"] == "completed")
        .await;
    assert_eq!(done["id"], id.as_str());
    assert_eq!(done["acked_lines"], 42);
    let idle = events
        .wait_for("machine", "the machine to go idle", |m| {
            m["active_job"].is_null() && m["status"]["state"] == "Idle"
        })
        .await;
    assert_eq!(idle["alarm"], Value::Null);
}

#[tokio::test]
async fn a_stopped_job_reports_its_final_state_too() {
    let h = Harness::start(slow()).await;
    let mut events = h.events().await;
    let id = h.run(&zigzag(200)).await;
    events
        .wait_for("job", "progress", |j| j["acked_lines"].as_u64() > Some(20))
        .await;
    h.action(&id, "stop").await;
    let job = events
        .wait_for("job", "the job to be stopped", |j| j["state"] == "stopped")
        .await;
    assert_eq!(job["id"], id.as_str());
}

#[tokio::test]
async fn a_client_that_joins_late_is_told_about_the_job_in_progress() {
    let h = Harness::start(slow()).await;
    let id = h.run(&zigzag(200)).await;
    h.wait_job(&id, "progress", |j| j["acked_lines"].as_u64() > Some(10))
        .await;

    let mut events = h.events().await;
    let mut seen = Vec::new();
    // Both the machine and the job are in the opening events.
    for _ in 0..2 {
        seen.push(events.next().await);
    }
    let names: Vec<_> = seen.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"machine") && names.contains(&"job"),
        "{names:?}"
    );
    let job = &seen.iter().find(|(n, _)| n == "job").unwrap().1;
    assert_eq!(job["id"], id.as_str());
    assert_eq!(job["state"], "running");
}

#[tokio::test]
async fn jogs_show_up_as_the_jog_state() {
    let h = Harness::start(SimConfig::grbl().block_duration(Duration::from_millis(500))).await;
    let mut events = h.events().await;
    events.next().await;
    let (status, _) = h
        .post_json("/machine/jog", &json!({"x": 10, "feed": 1000}))
        .await;
    assert_eq!(status, 202);
    events
        .wait_for("machine", "the Jog state", |m| {
            m["status"]["state"] == "Jog"
        })
        .await;
    events
        .wait_for("machine", "the move to finish", |m| {
            m["status"]["state"] == "Idle" && m["status"]["machine_position"][0] == 10.0
        })
        .await;
}

#[tokio::test]
async fn connection_loss_and_alarms_are_pushed() {
    let h = Harness::start(SimConfig::grbl()).await;
    let mut events = h.events().await;
    events.next().await;

    h.sim.trigger_alarm(2);
    let alarm = events
        .wait_for("machine", "the alarm", |m| m["alarm"] == 2)
        .await;
    assert!(alarm["alarm_message"].is_string());

    h.sim.disconnect();
    events
        .wait_for("machine", "the connection to drop", |m| {
            m["connection"] != "connected"
        })
        .await;
    h.wait_disconnected().await;
    h.sim.reconnect(true).unwrap();
    events
        .wait_for("machine", "the connection to return", |m| {
            m["connection"] == "connected" && m["ready"] == true
        })
        .await;
}

#[tokio::test]
async fn every_client_gets_the_same_events() {
    let h = Harness::start(slow()).await;
    let mut a = h.events().await;
    let mut b = h.events().await;
    let id = h.run(&zigzag(30)).await;
    for stream in [&mut a, &mut b] {
        let done = stream
            .wait_for("job", "the job to complete", |j| j["state"] == "completed")
            .await;
        assert_eq!(done["id"], id.as_str());
    }
}

#[tokio::test]
async fn a_client_that_never_reads_does_not_hold_anything_up() {
    let h = Harness::start(slow()).await;
    // Connected, and never read from.
    let _stalled = h.events().await;
    let id = h.run(&zigzag(60)).await;
    h.wait_job_state(&id, "completed").await;
    assert_eq!(h.job(&id).await["acked_lines"], 62);
}

#[tokio::test]
async fn ctrl_c_ends_open_streams_and_the_server_exits() {
    let mut h = Harness::start(SimConfig::grbl()).await;
    let mut events = h.events().await;
    events.next().await;
    let status = h.interrupt_server().await;
    assert!(status.success(), "{status:?}");
}
