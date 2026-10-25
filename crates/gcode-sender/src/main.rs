mod bounds;
mod controller;
mod gcode;
mod grbl;
mod http;
mod program;
mod settings;
mod store;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tracing::{info, warn};

/// A G-code sender for GRBL-family CNC controllers, driven over HTTP.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Serial port of the controller, such as /dev/ttyACM0.
    #[arg(long, env = "GCODE_SENDER_PORT")]
    port: String,

    /// Serial baud rate. LongMill controllers use 115200.
    #[arg(long, env = "GCODE_SENDER_BAUD", default_value_t = 115_200)]
    baud: u32,

    /// Address for the HTTP API and web UI, as an IP address and port. It
    /// must be a loopback address: the sender only serves the machine it runs
    /// on. Use port 0 to pick a free port.
    #[arg(long, env = "GCODE_SENDER_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,

    /// Where jobs are stored. Everything needed after a restart lives here.
    #[arg(long, env = "GCODE_SENDER_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Bytes of the controller's RX buffer to leave unused.
    #[arg(long, default_value_t = 8)]
    rx_reserve: usize,

    /// Status report interval in milliseconds.
    #[arg(long, default_value_t = 200)]
    poll_ms: u64,

    /// Delay between attempts to open the serial port, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    reconnect_ms: u64,

    /// Treat the connection as dead after this long without hearing from
    /// the controller, in milliseconds.
    #[arg(long, default_value_t = 3000)]
    silence_timeout_ms: u64,

    /// Treat an acknowledgement as lost when lines stay unacknowledged for
    /// this long while the machine is idle, in milliseconds. Keep it above
    /// the longest dwell (G4) in your programs.
    #[arg(long, default_value_t = 30_000)]
    ack_timeout_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gcode_sender=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();

    // The UI and the API are for the machine the controller is plugged into.
    // Refuse anything that could be reached from elsewhere, before the serial
    // port or the data directory is touched.
    let listen: SocketAddr = args.listen.parse().with_context(|| {
        format!(
            "--listen {} must be an IP address and port, such as 127.0.0.1:8080",
            args.listen
        )
    })?;
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "--listen must be a loopback address (127.0.0.1 or [::1]), not {listen}: \
         the sender and its web UI only serve the machine they run on"
    );

    let store = Arc::new(store::JobStore::open(&args.data_dir)?);
    for job in store.recover()? {
        warn!(id = %job.id, "job was active when the server last stopped; marked interrupted");
    }

    let controller = controller::spawn(
        controller::Config {
            port: args.port,
            baud: args.baud,
            rx_reserve: args.rx_reserve,
            poll_interval: Duration::from_millis(args.poll_ms),
            reconnect_interval: Duration::from_millis(args.reconnect_ms),
            stop_timeout: Duration::from_secs(5),
            silence_timeout: Duration::from_millis(args.silence_timeout_ms),
            ack_timeout: Duration::from_millis(args.ack_timeout_ms),
        },
        store.clone(),
    );

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let addr = listener.local_addr()?;
    info!(%addr, "HTTP API listening");
    // Printed on stdout so scripts and tests can find the port.
    println!("listening on {addr}");
    info!("web UI at http://{addr}/");

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let app = http::router(http::AppState {
        store,
        controller,
        shutdown,
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            // Event streams never end on their own.
            let _ = stop.send(true);
        })
        .await?;
    Ok(())
}
