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
mod tls;
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

    /// Address for this machine's own pages and API, over plain HTTP. It must
    /// be a loopback address, so only this machine can reach it. Use port 0 to
    /// pick a free port.
    #[arg(long, env = "GCODE_SENDER_LISTEN", default_value = "127.0.0.1:8080")]
    listen: String,

    /// Address for remote browsers, over HTTPS with a self-signed certificate.
    /// They pair with the code shown on this machine before they can use it.
    #[arg(long, env = "GCODE_SENDER_REMOTE", default_value = "0.0.0.0:8443")]
    remote: String,

    /// Do not accept remote browsers. Only --listen is served.
    #[arg(long, env = "GCODE_SENDER_NO_REMOTE")]
    no_remote: bool,

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

    // Checked before the serial port or the data directory is touched.
    let listen: SocketAddr = args.listen.parse().with_context(|| {
        format!(
            "--listen {} must be an IP address and port, such as 127.0.0.1:8080",
            args.listen
        )
    })?;
    anyhow::ensure!(
        listen.ip().is_loopback(),
        "--listen serves plain HTTP to this machine only, so it must be a loopback address \
         (127.0.0.1 or [::1]), not {listen}; remote browsers use --remote"
    );
    let remote: Option<SocketAddr> = if args.no_remote {
        None
    } else {
        Some(args.remote.parse().with_context(|| {
            format!(
                "--remote {} must be an IP address and port, such as 0.0.0.0:8443",
                args.remote
            )
        })?)
    };

    let store = Arc::new(store::JobStore::open(&args.data_dir)?);
    for job in store.recover()? {
        warn!(id = %job.id, "job was active when the server last stopped; marked interrupted");
    }
    let pairing = Arc::new(pairing::Pairing::open(&args.data_dir)?);

    let tls = match remote {
        Some(_) => {
            // Chosen here rather than left to the features of the dependencies.
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();
            let (cert, key) = tls::load_or_create(&args.data_dir)?;
            Some(axum_server::tls_rustls::RustlsConfig::from_pem(cert, key).await?)
        }
        None => None,
    };

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

    let local_listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let local_addr = local_listener.local_addr()?;
    info!(%local_addr, "HTTP API listening");
    // Printed on stdout so scripts and tests can find the ports.
    println!("listening on {local_addr}");
    info!("web UI at http://{local_addr}/");

    let remote_listener = match remote {
        Some(addr) => {
            let listener =
                std::net::TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
            listener.set_nonblocking(true)?;
            let bound = listener.local_addr()?;
            info!(%bound, "HTTPS listening for remote browsers");
            println!("remote listening on https://{bound}");
            Some((listener, bound))
        }
        None => None,
    };
    if let Some((_, bound)) = &remote_listener {
        let code = pairing.view().code;
        info!("remote browsers pair at https://{bound}/ with the code {code}");
    }

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        // Event streams never end on their own.
        let _ = stop.send(true);
    });

    let state = |local: bool| http::AppState {
        store: store.clone(),
        controller: controller.clone(),
        pairing: pairing.clone(),
        shutdown: shutdown.clone(),
        local,
    };
    let mut ended = shutdown.clone();
    let local_server = axum::serve(local_listener, http::router(state(true)))
        .with_graceful_shutdown(async move {
            let _ = ended.wait_for(|stopping| *stopping).await;
        });

    let remote_server = async {
        let Some((listener, _)) = remote_listener else {
            return Ok(());
        };
        let tls = tls.expect("the certificate is loaded when remote access is on");
        let handle = axum_server::Handle::new();
        let stopping = handle.clone();
        let mut ended = shutdown.clone();
        tokio::spawn(async move {
            let _ = ended.wait_for(|stopping| *stopping).await;
            stopping.graceful_shutdown(Some(Duration::from_secs(2)));
        });
        axum_server::from_tcp_rustls(listener, tls)
            .handle(handle)
            .serve(http::router(state(false)).into_make_service())
            .await
    };

    let (local_result, remote_result) = tokio::join!(local_server, remote_server);
    local_result?;
    remote_result?;
    Ok(())
}
