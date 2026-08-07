//! squawk intercom server.
//!
//! Hosts the mix engine and serves the browser UI. Intended to be wrapped by
//! `av-launcher` as a tray app, the same way srt-router and flock are — this binary
//! stays headless so that the tray shell is one shared thing rather than one per app.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use serde::Serialize;
use squawk_core::Config;
use squawk_server::{app, AppState};

#[derive(Parser, Debug, Serialize)]
#[command(name = "squawk-server", version, about = "squawk partyline intercom server")]
struct Args {
    /// Config file to load and save. Created from a default if it does not exist.
    #[arg(short, long, default_value = "squawk.toml")]
    config: PathBuf,

    /// Address to bind the web UI to.
    #[arg(short, long, default_value = "127.0.0.1")]
    bind: String,

    #[arg(short, long, default_value_t = 8477)]
    port: u16,

    /// Local address of the NIC on the audio network, which enables real AES67.
    ///
    /// Required rather than auto-detected on purpose: on a multi-homed machine the
    /// routing table usually prefers the office LAN, and multicast sent the wrong way
    /// reports success and is heard by nothing. Omit it to run on synthesised tones.
    #[arg(short, long)]
    interface: Option<Ipv4Addr>,

    /// Jitter buffer depth in packets, which at the default 1 ms packet time is also
    /// milliseconds of added latency.
    #[arg(long, default_value_t = 2)]
    jitter_depth: usize,
}

/// A system with nothing in it but one partyline, so a first run has somewhere to
/// click rather than an empty screen and no obvious next step.
fn starter_config() -> Config {
    let mut cfg = Config::default();
    cfg.system.name = "squawk".to_owned();
    cfg.partylines
        .push(squawk_core::Partyline::new("prod", "Production"));
    cfg
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // First thing in main, and the guard is held for the life of the process — see
    // crates/diag: dropping it silently stops the log file being written.
    let _guard = diag::init(
        diag::Options::new("squawk-server", "SQUAWK", env!("CARGO_PKG_VERSION"))
            .with_default_filter("info,tower_http=warn")
            .with_config(&args),
    )?;

    let config = if args.config.exists() {
        let text = std::fs::read_to_string(&args.config)
            .with_context(|| format!("reading {}", args.config.display()))?;
        let cfg = Config::from_toml(&text)
            .with_context(|| format!("parsing {}", args.config.display()))?;

        for problem in cfg.validate() {
            match problem.severity {
                squawk_core::Severity::Error => tracing::error!("{}", problem.message),
                squawk_core::Severity::Warning => tracing::warn!("{}", problem.message),
            }
        }
        if !cfg.is_valid() {
            anyhow::bail!(
                "{} has validation errors and cannot be served; fix them and restart",
                args.config.display()
            );
        }
        cfg
    } else {
        tracing::info!(path = %args.config.display(), "no config found; starting a new one");
        let cfg = starter_config();
        std::fs::write(&args.config, cfg.to_toml()?)
            .with_context(|| format!("writing {}", args.config.display()))?;
        cfg
    };

    tracing::info!(
        endpoints = config.endpoints.len(),
        partylines = config.partylines.len(),
        aes67_streams = config.aes67_stream_count(),
        "loaded config"
    );

    let transport = args.interface.map(|iface| squawk_server::host::TransportOptions {
        iface,
        jitter_depth: args.jitter_depth,
    });
    if transport.is_none() {
        tracing::warn!(
            "no --interface given: running on synthesised tones, sending nothing to the network"
        );
    }

    let state = AppState::with_transport(config, Some(args.config.clone()), transport);
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .with_context(|| format!("invalid bind address {}:{}", args.bind, args.port))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!("squawk server on http://{addr}");
    println!("squawk server on http://{addr}");

    axum::serve(listener, app(state)).await?;
    Ok(())
}
