//! Connecting to the controller and identifying its firmware.

mod common;

use common::{Harness, wait_for};
use grbl_sim::SimConfig;
use reqwest::StatusCode;

#[tokio::test]
async fn health_check_answers() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let (status, body) = h.get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn identifies_grbl_longboard() {
    let h = Harness::start(SimConfig::grbl()).await;
    let m = h.machine().await;
    assert_eq!(m["connection"], "connected");
    assert_eq!(m["firmware"]["kind"], "grbl");
    assert_eq!(m["firmware"]["version"], "1.1h");
    assert_eq!(m["rx_buffer_size"], 128);
    assert!(h.sim.received_lines().contains(&"$I".to_string()));
}

#[tokio::test]
async fn identifies_grblhal_superlongboard() {
    let h = Harness::start(SimConfig::grblhal()).await;
    let m = h.machine().await;
    assert_eq!(m["firmware"]["kind"], "grblhal");
    assert_eq!(m["rx_buffer_size"], 1024);
}

#[tokio::test]
async fn identifies_controller_that_printed_no_banner() {
    // The SLB does not reset when the port opens, so no banner arrives.
    let mut config = SimConfig::grblhal();
    config.banner_on_start = false;
    let h = Harness::start(config).await;
    assert_eq!(h.machine().await["firmware"]["kind"], "grblhal");
}

#[tokio::test]
async fn reports_machine_status() {
    let h = Harness::start(SimConfig::grbl()).await;
    let m = h.machine().await;
    assert_eq!(m["status"]["state"], "Idle");
    assert_eq!(
        m["status"]["machine_position"],
        serde_json::json!([0.0, 0.0, 0.0])
    );
    // Status is polled continuously with the `?` realtime command.
    wait_for("several status queries", || async {
        (h.sim
            .realtime_bytes()
            .iter()
            .filter(|&&b| b == b'?')
            .count()
            > 3)
        .then_some(())
    })
    .await;
}

#[tokio::test]
async fn reports_disconnected_when_port_is_missing() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    // Remove the port out from under a fresh server.
    let port = h.sim.port().to_path_buf();
    let target = std::fs::read_link(&port).unwrap();
    std::fs::remove_file(&port).unwrap();
    let mut h = h;
    h.restart_server().await;
    let m = h
        .wait_machine("a failed connection", |m| m["connection"] == "disconnected")
        .await;
    assert!(m["last_error"].as_str().unwrap().contains("serial port"));
    assert_eq!(m["ready"], false);

    // Once the port appears, the server connects on its own.
    std::os::unix::fs::symlink(target, &port).unwrap();
    h.wait_ready().await;
}
