mod bounds;
mod controller;
mod gcode;
mod grbl;
mod http;
mod pairing;
mod position;
mod program;
mod settings;
mod store;
mod web;
mod work;

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

    /// Address for the HTTP API and web UI, as an IP address and port. Use
    /// 127.0.0.1 to serve only this machine. Other computers reach the sender
    /// by pairing a browser with the code shown on its screen. Use port 0 to
    /// pick a free port.
    #[arg(long, env = "GCODE_SENDER_LISTEN", default_value = "0.0.0.0:8080")]
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

    let listen: SocketAddr = args.listen.parse().with_context(|| {
        format!(
            "--listen {} must be an IP address and port, such as 0.0.0.0:8080",
            args.listen
        )
    })?;

    let store = Arc::new(store::JobStore::open(&args.data_dir)?);
    for job in store.recover()? {
        warn!(id = %job.id, "job was active when the server last stopped; marked interrupted");
    }
    let pairing = Arc::new(pairing::Pairing::open(&args.data_dir)?);
    if !listen.ip().is_loopback() {
        let code = pairing.view().code;
        info!("other computers can pair with the code {code} (shown on this machine's page)");
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
        pairing,
        shutdown,
    });
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        // Event streams never end on their own.
        let _ = stop.send(true);
    })
    .await?;
    Ok(())
}
