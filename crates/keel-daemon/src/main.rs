//! `keel-daemon`: headless Keel. Hosts the profile's library (and keel-net when
//! Settings → Devices, `[devices] enabled`, in the profile's config.toml) and serves the `keel-api`
//! operations as JSON-RPC 2.0 on a per-user local socket, optionally on a WebSocket.
//! See docs/api.md.

mod server;
mod share;
mod web;
mod ws;

use clap::Parser;
use keel_api::client::Client;
use keel_api::config::{valid_profile, HostConfig, DEFAULT_PROFILE, PROFILE_RULE};
use serde_json::Value;
use std::net::SocketAddr;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "keel-daemon",
    version,
    about = "Headless Keel: the library behind JSON-RPC"
)]
struct Args {
    /// Settings profile: <config dir>/profiles/<NAME>.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_PROFILE, value_parser = profile_name)]
    profile: String,
    /// Also serve JSON-RPC over a WebSocket on this address (e.g. 127.0.0.1:7420); clients
    /// send `Authorization: Bearer <token>` from <config dir>/daemon.token.
    #[arg(long, value_name = "ADDR")]
    ws: Option<SocketAddr>,
    /// Serve the browser client on this address (default 127.0.0.1:7421): the page asks
    /// for the token from <config dir>/daemon.token.
    #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = "127.0.0.1:7421")]
    web: Option<SocketAddr>,
    /// Allow --ws or --web on a non-loopback address (use TLS or a private network).
    #[arg(long)]
    ws_allow_remote: bool,
    /// A host name --web also answers to (repeatable): a tailnet name for a remote bind,
    /// or the name a TLS reverse proxy in front of a loopback bind passes on. Loopback
    /// names (loopback bind) or the bound IP (remote bind) always work.
    #[arg(long = "web-host", value_name = "NAME")]
    web_host: Vec<String>,
    /// Print whether a daemon runs for the profile (exit 0 when it does, 1 when not).
    #[arg(long)]
    status: bool,
}

/// Profile names become a folder name (`keel_api::config::valid_profile`).
fn profile_name(name: &str) -> Result<String, String> {
    match valid_profile(name) {
        true => Ok(name.to_owned()),
        false => Err(PROFILE_RULE.into()),
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cfg = match HostConfig::load(&args.profile) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("keel-daemon: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    if args.status {
        return status(&cfg);
    }
    if args.ws_allow_remote && args.ws.is_none() && args.web.is_none() {
        eprintln!("keel-daemon: --ws-allow-remote needs --ws or --web");
        return ExitCode::FAILURE;
    }
    if !args.web_host.is_empty() && args.web.is_none() {
        eprintln!("keel-daemon: --web-host needs --web");
        return ExitCode::FAILURE;
    }
    let daemon = match server::Daemon::start(server::Options {
        cfg: cfg.clone(),
        ws: args.ws,
        web: args.web,
        ws_allow_remote: args.ws_allow_remote,
        web_hosts: args.web_host,
        net: None,
    }) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("keel-daemon: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let (tx, signals) = crossbeam_channel::bounded(1);
    if let Err(e) = ctrlc::set_handler(move || {
        let _ = tx.try_send(());
    }) {
        tracing::warn!("no signal handler: {e}");
    }
    eprintln!(
        "keel-daemon: serving library {} for profile {} (pid {}){}",
        cfg.library,
        cfg.profile,
        std::process::id(),
        daemon
            .ws_addr()
            .map(|a| format!(", WebSocket on ws://{a}"))
            .unwrap_or_default()
            + &daemon
                .web_addr()
                .map(|a| format!(", web client on http://{a}/"))
                .unwrap_or_default()
    );
    crossbeam_channel::select! {
        recv(signals) -> _ => tracing::info!("signal: stopping"),
        recv(daemon.shutdown_requests()) -> _ => tracing::info!("daemon.shutdown: stopping"),
    }
    daemon.shutdown();
    ExitCode::SUCCESS
}

fn status(cfg: &HostConfig) -> ExitCode {
    let running = Client::connect(&cfg.socket_name())
        .ok()
        .and_then(|mut c| c.call("version", Value::Null).ok());
    match running {
        Some(v) => {
            println!(
                "keel-daemon is running for profile {} (pid {}, library {})",
                cfg.profile,
                v["pid"],
                v["library"].as_str().unwrap_or("?")
            );
            ExitCode::SUCCESS
        }
        None => {
            println!("keel-daemon is not running for profile {}", cfg.profile);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;
