//! The machine's own pages reach the sender without a code. Requests from
//! other places need a paired browser, and pages of other sites are refused
//! outright.

mod common;

use std::process::Command;

use common::Harness;
use grbl_sim::SimConfig;
use reqwest::StatusCode;
use serde_json::Value;

#[test]
fn the_listen_address_must_be_an_ip_address_and_port() {
    let dir = tempfile::tempdir().unwrap();
    for listen in ["localhost:8080", "8080", "127.0.0.1"] {
        let out = Command::new(env!("CARGO_BIN_EXE_gcode-sender"))
            .args(["--port", "/dev/null", "--listen", listen, "--data-dir"])
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{listen} should be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("IP address"),
            "{listen}: unexpected message: {err}"
        );
    }
}

#[tokio::test]
async fn the_web_ui_is_served_with_a_strict_policy() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = reqwest::Client::new();

    let res = client.get(format!("{}/", h.base)).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res.headers()["content-type"].to_str().unwrap().to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let csp = res.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(
        res.text()
            .await
            .unwrap()
            .contains("<title>G-code Sender</title>")
    );

    // Everything the page loads comes from the sender itself, so it works
    // with no internet access.
    for (path, kind) in [
        ("/app.js", "javascript"),
        ("/style.css", "text/css"),
        ("/vendor/alpine-csp-3.17.5.min.js", "javascript"),
        ("/vendor/d3-7.9.0.min.js", "javascript"),
        ("/vendor/fonts/DSEG7Classic-Bold.woff2", "font/woff2"),
    ] {
        let res = client
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        let content_type = res.headers()["content-type"].to_str().unwrap();
        assert!(content_type.contains(kind), "{path}: {content_type}");
    }
}

#[tokio::test]
async fn a_request_under_another_host_name_needs_a_pairing_code() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = reqwest::Client::new();

    // What a page on another site looks like after DNS rebinding points its
    // name at this machine. It is not this machine's own page, so it has no
    // token and gets no access.
    let res = client
        .get(format!("{}/jobs", h.base))
        .header("host", "evil.example:8080")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["pairing"], true);
}

#[tokio::test]
async fn a_page_from_another_site_cannot_send_commands() {
    let h = Harness::start(SimConfig::grbl()).await;
    let client = reqwest::Client::new();

    // A form post from another site arrives as text/plain with an Origin the
    // browser cannot forge. It must be refused before any handler runs.
    for origin in ["http://evil.example", "https://localhost:8080", "null"] {
        let res = client
            .post(format!("{}/machine/jog", h.base))
            .header("origin", origin)
            .header("content-type", "text/plain")
            .body(r#"{"x": 10, "feed": 1000}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "origin {origin}");
    }
    let res = client
        .post(format!("{}/jobs?name=evil.nc", h.base))
        .header("origin", "http://evil.example")
        .body("G1 X10 F1000")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let (_, jobs) = h.get("/jobs").await;
    assert_eq!(jobs, Value::Array(vec![]));
}

#[tokio::test]
async fn the_machine_own_pages_and_tools_still_work() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = reqwest::Client::new();
    let authority = h.base.trim_start_matches("http://");
    let port = authority.rsplit(':').next().unwrap();

    // The page's own origin, and the same machine by its name.
    let res = client
        .get(format!("{}/health", h.base))
        .header("origin", format!("http://{authority}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = client
        .get(format!("http://localhost:{port}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
