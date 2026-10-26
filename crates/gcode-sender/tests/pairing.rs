//! Pairing: a browser on another computer gets in with the code shown on the
//! sender's screen, and keeps access until the machine revokes it.
//!
//! The test runs on this machine, so the connection is loopback. A request
//! that names the sender by a host name other than loopback is what a remote
//! browser looks like to the sender.

mod common;

use common::Harness;
use grbl_sim::SimConfig;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde_json::{Value, json};

const REMOTE_HOST: &str = "sender.example:8080";

/// A request as a browser on another computer sends it.
fn remote(client: &Client, method: Method, url: String) -> RequestBuilder {
    client.request(method, url).header("host", REMOTE_HOST)
}

/// The code showing on the machine, read from its own screen.
async fn code_on_screen(h: &Harness) -> String {
    let (status, view) = h.get("/pairing").await;
    assert_eq!(status, StatusCode::OK);
    view["code"].as_str().unwrap().to_string()
}

/// Pairs a browser with the code on screen and returns its token.
async fn pair(h: &Harness, client: &Client) -> String {
    let code = code_on_screen(h).await;
    let res = remote(client, Method::POST, format!("{}/pairing", h.base))
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

fn with_token(req: RequestBuilder, token: &str) -> RequestBuilder {
    req.header("cookie", format!("gcode_sender_session={token}"))
}

#[tokio::test]
async fn a_remote_browser_is_asked_to_pair_and_can_still_load_the_page() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = Client::new();

    let res = remote(&client, Method::GET, format!("{}/machine", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["pairing"], true);

    let res = remote(&client, Method::GET, format!("{}/", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = remote(&client, Method::GET, format!("{}/session", h.base))
        .send()
        .await
        .unwrap();
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["access"], "none");
}

#[tokio::test]
async fn the_code_pairs_a_browser_once_and_the_token_then_opens_the_api() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = Client::new();
    let code = code_on_screen(&h).await;
    assert_eq!(code.len(), 7, "shown as two groups of three: {code}");

    // A wrong code is refused, and says how many tries are left.
    let wrong = if code == "000 000" {
        "111 111"
    } else {
        "000 000"
    };
    let res = remote(&client, Method::POST, format!("{}/pairing", h.base))
        .json(&json!({ "code": wrong }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: Value = res.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("4 more tries"));

    // The right code gives a cookie and a token.
    let res = remote(&client, Method::POST, format!("{}/pairing", h.base))
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let cookie = res
        .headers()
        .get("set-cookie")
        .expect("a session cookie")
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    let body: Value = res.json().await.unwrap();
    let token = body["token"].as_str().unwrap().to_string();

    // The cookie opens the API, and so does a bearer token.
    let res = with_token(
        remote(&client, Method::GET, format!("{}/machine", h.base)),
        &token,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = remote(&client, Method::GET, format!("{}/machine", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // A code works once.
    let res = remote(&client, Method::POST, format!("{}/pairing", h.base))
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Only the machine's own screen manages pairings.
    let res = with_token(
        remote(&client, Method::GET, format!("{}/pairing", h.base)),
        &token,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = with_token(
        remote(&client, Method::GET, format!("{}/session", h.base)),
        &token,
    )
    .send()
    .await
    .unwrap();
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["access"], "paired");

    // The machine lists the browser and can revoke it.
    let (_, view) = h.get("/pairing").await;
    let devices = view["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    let id = devices[0]["id"].as_str().unwrap().to_string();
    let (status, _) = h.delete(&format!("/pairing/devices/{id}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let res = with_token(
        remote(&client, Method::GET, format!("{}/machine", h.base)),
        &token,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let (status, _) = h.delete(&format!("/pairing/devices/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn five_wrong_guesses_use_the_code_up() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = Client::new();
    let code = code_on_screen(&h).await;
    let wrong = if code == "000 000" {
        "111 111"
    } else {
        "000 000"
    };

    let mut last = None;
    for _ in 0..5 {
        let res = remote(&client, Method::POST, format!("{}/pairing", h.base))
            .json(&json!({ "code": wrong }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        last = Some(res.json::<Value>().await.unwrap());
    }
    let message = last.unwrap()["error"].as_str().unwrap().to_string();
    assert!(message.contains("no tries are left"), "{message}");

    // Even the right code is dead now; the machine has to show a new one.
    let res = remote(&client, Method::POST, format!("{}/pairing", h.base))
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_cross_site_page_is_refused_even_with_a_paired_token() {
    let h = Harness::start_unready(SimConfig::grbl()).await;
    let client = Client::new();
    let token = pair(&h, &client).await;
    let jog = json!({ "x": 10, "feed": 1000 });

    // Another site's page, made in the visitor's browser.
    let res = with_token(
        remote(&client, Method::POST, format!("{}/machine/jog", h.base)),
        &token,
    )
    .header("origin", "http://evil.example")
    .header("sec-fetch-site", "cross-site")
    .json(&jog)
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Even with no Origin header, a request the browser calls cross-site is refused.
    let res = with_token(
        remote(&client, Method::POST, format!("{}/machine/jog", h.base)),
        &token,
    )
    .header("sec-fetch-site", "cross-site")
    .json(&jog)
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // The sender's own page, as the paired browser loads it.
    let res = with_token(
        remote(&client, Method::GET, format!("{}/machine", h.base)),
        &token,
    )
    .header("origin", format!("http://{REMOTE_HOST}"))
    .header("sec-fetch-site", "same-origin")
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_paired_browser_stays_paired_across_a_restart() {
    let mut h = Harness::start_unready(SimConfig::grbl()).await;
    let client = Client::new();
    let token = pair(&h, &client).await;

    h.restart_server().await;

    let res = with_token(
        remote(&client, Method::GET, format!("{}/machine", h.base)),
        &token,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let stored = std::fs::read_to_string(h.data_dir().join("devices.json")).unwrap();
    assert!(
        !stored.contains(&token),
        "the token itself must not be stored"
    );
}
