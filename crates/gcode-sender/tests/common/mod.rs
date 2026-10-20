//! Acceptance test harness.
//!
//! Each test gets a simulated controller on its own pseudo-terminal and a
//! real `gcode-sender` process pointed at it. Tests talk to the server only
//! over HTTP and look at the controller only through the simulator, so they
//! exercise the same boundaries the real machine and its clients see.
//!
//! Set `GCODE_SENDER_TEST_LOG=1` to see the server's logs.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use grbl_sim::{SimConfig, Simulator};
use reqwest::StatusCode;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Harness {
    pub sim: Simulator,
    server: Option<Child>,
    pub base: String,
    client: reqwest::Client,
    data_dir: PathBuf,
    _dir: TempDir,
}

impl Harness {
    /// Starts a simulator and a server, and waits for the controller to be
    /// identified.
    pub async fn start(config: SimConfig) -> Self {
        let h = Self::start_unready(config).await;
        h.wait_ready().await;
        h
    }

    pub async fn start_unready(config: SimConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sim = Simulator::start(config, dir.path().join("ttySIM")).unwrap();
        let data_dir = dir.path().join("data");
        let mut h = Self {
            sim,
            server: None,
            base: String::new(),
            client: reqwest::Client::new(),
            data_dir,
            _dir: dir,
        };
        h.spawn_server().await;
        h
    }

    async fn spawn_server(&mut self) {
        let log = std::env::var_os("GCODE_SENDER_TEST_LOG").is_some();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gcode-sender"));
        cmd.arg("--port")
            .arg(self.sim.port())
            .args([
                "--listen",
                "127.0.0.1:0",
                "--poll-ms",
                "50",
                "--reconnect-ms",
                "200",
                "--silence-timeout-ms",
                "1500",
                "--ack-timeout-ms",
                "1000",
            ])
            .arg("--data-dir")
            .arg(&self.data_dir)
            .env("RUST_LOG", "gcode_sender=debug")
            .stdout(Stdio::piped())
            .stderr(if log { Stdio::inherit() } else { Stdio::null() })
            .kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn gcode-sender");
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(TIMEOUT, lines.next_line())
            .await
            .expect("server did not start in time")
            .unwrap()
            .expect("server exited before listening");
        let addr = line.strip_prefix("listening on ").expect("listen line");
        self.base = format!("http://{addr}");
        self.server = Some(child);
    }

    /// Kills the server abruptly, as a crash or power cut would.
    pub async fn kill_server(&mut self) {
        if let Some(mut child) = self.server.take() {
            child.kill().await.unwrap();
            self.sim.release_port();
        }
    }

    /// Starts a new server process on the same data directory.
    pub async fn restart_server(&mut self) {
        self.kill_server().await;
        self.spawn_server().await;
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        let res = self.client.get(self.url(path)).send().await.unwrap();
        decode(res).await
    }

    pub async fn post(&self, path: &str) -> (StatusCode, Value) {
        let res = self.client.post(self.url(path)).send().await.unwrap();
        decode(res).await
    }

    pub async fn post_body(&self, path: &str, body: &str) -> (StatusCode, Value) {
        let res = self
            .client
            .post(self.url(path))
            .header("content-type", "text/plain")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        decode(res).await
    }

    pub async fn put_json(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        let res = self
            .client
            .put(self.url(path))
            .json(body)
            .send()
            .await
            .unwrap();
        decode(res).await
    }

    pub async fn delete(&self, path: &str) -> (StatusCode, Value) {
        let res = self.client.delete(self.url(path)).send().await.unwrap();
        decode(res).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn machine(&self) -> Value {
        let (status, body) = self.get("/machine").await;
        assert_eq!(status, StatusCode::OK);
        body
    }

    pub async fn wait_ready(&self) -> Value {
        self.wait_machine("the controller to be ready", |m| {
            m["ready"] == true && m["status"]["state"] == "Idle"
        })
        .await
    }

    pub async fn wait_machine(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        wait_for(what, || async {
            let m = self.machine().await;
            pred(&m).then_some(m)
        })
        .await
    }

    /// Uploads a program and returns its job record.
    pub async fn upload(&self, gcode: &str) -> Value {
        let (status, job) = self.post_body("/jobs", gcode).await;
        assert_eq!(status, StatusCode::CREATED, "upload failed: {job}");
        job
    }

    pub async fn job(&self, id: &str) -> Value {
        let (status, job) = self.get(&format!("/jobs/{id}")).await;
        assert_eq!(status, StatusCode::OK, "job lookup failed: {job}");
        job
    }

    pub async fn action(&self, id: &str, action: &str) -> Value {
        let (status, job) = self.post(&format!("/jobs/{id}/{action}")).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{action} failed: {job}");
        job
    }

    pub async fn wait_job(&self, id: &str, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        wait_for(what, || async {
            let job = self.job(id).await;
            pred(&job).then_some(job)
        })
        .await
    }

    pub async fn wait_job_state(&self, id: &str, state: &str) -> Value {
        self.wait_job(id, &format!("job to be {state}"), |j| j["state"] == state)
            .await
    }

    pub async fn wait_idle(&self) -> Value {
        self.wait_machine("the machine to be idle", |m| {
            m["ready"] == true && m["status"]["state"] == "Idle"
        })
        .await
    }

    /// Waits for the server to notice the link is gone. A real serial port
    /// clears its exclusive lock when the server closes it; the simulator's
    /// pseudo-terminal needs to be told.
    pub async fn wait_disconnected(&self) -> Value {
        let m = self
            .wait_machine("the connection to drop", |m| m["connection"] != "connected")
            .await;
        self.sim.release_port();
        m
    }

    /// Uploads and starts a program, returning the job id.
    pub async fn run(&self, gcode: &str) -> String {
        let id = self.upload(gcode).await["id"].as_str().unwrap().to_string();
        self.action(&id, "start").await;
        id
    }
}

async fn decode(res: reqwest::Response) -> (StatusCode, Value) {
    let status = res.status();
    let text = res.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

/// Polls until `check` returns a value, failing the test after [`TIMEOUT`].
pub async fn wait_for<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Some(v) = check().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A program of `n` small moves along X, with comments and blank lines
/// mixed in the way CAM output has them.
pub fn zigzag(n: usize) -> String {
    let mut g = String::from("; generated test program\n%\nG21 G90\n\n(start)\n");
    for i in 1..=n {
        let y = if i % 2 == 0 { 1.0 } else { 0.0 };
        g.push_str(&format!(
            "G1 X{:.3} Y{y:.3} F1000 ; move {i}\n",
            i as f64 * 0.5
        ));
    }
    g.push_str("M2\n%\n");
    g
}

/// The lines a correct sender delivers for `zigzag(n)`: comments, blank
/// lines and `%` are gone.
pub fn zigzag_lines(n: usize) -> Vec<String> {
    let mut lines = vec!["G21 G90".to_string()];
    for i in 1..=n {
        let y = if i % 2 == 0 { 1.0 } else { 0.0 };
        lines.push(format!("G1 X{:.3} Y{y:.3} F1000", i as f64 * 0.5));
    }
    lines.push("M2".into());
    lines
}

/// File line number of the prepared line at `index` in `zigzag(n)`.
pub fn zigzag_file_line(index: usize) -> usize {
    if index == 0 { 3 } else { 5 + index }
}

/// Lines the controller was sent that are part of a program, not queries.
pub fn program_lines(sim: &Simulator) -> Vec<String> {
    sim.received_lines()
        .into_iter()
        .filter(|l| !l.starts_with('$'))
        .collect()
}
