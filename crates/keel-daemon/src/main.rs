//! `keel-daemon`: headless Keel. Hosts the profile's library (and keel-net when
//! Settings → Devices, `[devices] enabled`, in the profile's config.toml) and serves the `keel-api`
//! operations as JSON-RPC 2.0 on a per-user local socket, optionally on a WebSocket.
//! See docs/api.md.

use clap::Parser;
use keel_api::client::Client;
use keel_api::config::{valid_profile, HostConfig, DEFAULT_PROFILE, PROFILE_RULE};
use keel_daemon::server;
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
    /// Store the HTTP login for tools written for Everything's HTTP server (Basic auth on
    /// --web's search) in the OS keychain: this user name, the password read from the
    /// first line of standard input. Then exit.
    #[arg(long, value_name = "USER", conflicts_with = "clear_http_login")]
    set_http_login: Option<String>,
    /// Remove the HTTP login from the OS keychain, then exit.
    #[arg(long)]
    clear_http_login: bool,
    /// Serve the ETP/FTP server for Everything clients on this address (default: [etp]
    /// bind in the profile's config.toml, else 127.0.0.1:21 on Windows, 127.0.0.1:2121
    /// elsewhere), even when [etp] enabled is off. Needs [[etp.roots]] and the ETP login.
    #[arg(long, value_name = "ADDR", num_args = 0..=1)]
    etp: Option<Option<SocketAddr>>,
    /// Store the ETP login in the OS keychain: this user name, the password read from the
    /// first line of standard input. Then exit.
    #[arg(long, value_name = "USER", conflicts_with = "clear_etp_login")]
    set_etp_login: Option<String>,
    /// Remove the ETP login from the OS keychain, then exit.
    #[arg(long)]
    clear_etp_login: bool,
}

/// Profile names become a folder name (`keel_api::config::valid_profile`).
fn profile_name(name: &str) -> Result<String, String> {
    match valid_profile(name) {
        true => Ok(name.to_owned()),
        false => Err(PROFILE_RULE.into()),
    }
}

/// `RUST_LOG` (default `info`) plus the directives that keep secrets out of the log
/// (`keel_api::crash::log_filter`).
fn filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::new(keel_api::crash::log_filter(rust_log.unwrap_or("info")))
}

fn main() -> ExitCode {
    // `keel.portable` beside the executable: settings, libraries and index beside it (as
    // for the window). First, before any thread starts.
    keel_core::portable::apply(keel_api::private::prepare_portable);
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_writer(|| keel_api::crash::tee(std::io::stderr()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(filter(std::env::var("RUST_LOG").ok().as_deref()))
        .init();
    if let Some(notice) = keel_core::portable::notice() {
        tracing::warn!("{notice}");
    }
    // Panics write <data dir>/crashes/<time>.txt (scrubbed) and log where.
    keel_api::crash::install_panic_hook("keel-daemon", keel_api::crash::dir());
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
    if args.set_http_login.is_some() || args.clear_http_login {
        return http_login(&cfg, args.set_http_login.as_deref(), Login::Http);
    }
    if args.set_etp_login.is_some() || args.clear_etp_login {
        return http_login(&cfg, args.set_etp_login.as_deref(), Login::Etp);
    }
    // PDF previews (`preview.render`, the web client): pdfium next to this executable,
    // where the release archives and the build's deps step put it, as for the window.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(std::path::Path::parent)
    {
        keel_preview::init_pdfium(dir);
    }
    if args.ws_allow_remote && args.ws.is_none() && args.web.is_none() {
        eprintln!("keel-daemon: --ws-allow-remote needs --ws or --web");
        return ExitCode::FAILURE;
    }
    // A non-loopback --web needs `[web] allow_plaintext = true` (`Daemon::start` refuses it
    // otherwise and warns once it serves).
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
    if args.etp.is_some() || keel_daemon::etp::enabled(&cfg) {
        if let Err(e) = daemon.start_etp(args.etp.flatten()) {
            eprintln!("keel-daemon: ETP server: {e:#}");
            daemon.shutdown();
            return ExitCode::FAILURE;
        }
    }
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
            + &daemon
                .etp_addr()
                .map(|a| format!(", ETP server on ftp://{a}/"))
                .unwrap_or_default()
    );
    crossbeam_channel::select! {
        recv(signals) -> _ => tracing::info!("signal: stopping"),
        recv(daemon.shutdown_requests()) -> _ => tracing::info!("daemon.shutdown: stopping"),
    }
    daemon.shutdown();
    ExitCode::SUCCESS
}

/// Which keychain login `--set-*-login` / `--clear-*-login` change.
#[derive(Clone, Copy)]
enum Login {
    Http,
    Etp,
}

/// `--set-http-login USER` / `--set-etp-login USER` (the password from stdin), or
/// `--clear-http-login` / `--clear-etp-login`.
fn http_login(cfg: &HostConfig, user: Option<&str>, which: Login) -> ExitCode {
    use keel_daemon::{etp, everything};
    let store = keel_vfs::cloud::KeyringStore;
    let (label, set, clear): (&str, SetLogin, ClearLogin) = match which {
        Login::Http => ("HTTP", everything::set_login, everything::clear_login),
        Login::Etp => ("ETP", etp::set_login, etp::clear_login),
    };
    let done = match user {
        Some(user) => {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(_) => {
                    let password = line.trim_end_matches(['\r', '\n']);
                    set(&store, &cfg.profile, user, password).map(|()| {
                        format!("{label} login for {user} stored (profile {})", cfg.profile)
                    })
                }
                Err(e) => Err(e.into()),
            }
        }
        None => clear(&store, &cfg.profile)
            .map(|()| format!("{label} login removed (profile {})", cfg.profile)),
    };
    match done {
        Ok(said) => {
            println!("{said}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("keel-daemon: {e:#}");
            ExitCode::FAILURE
        }
    }
}

type SetLogin = fn(&dyn keel_vfs::cloud::SecretStore, &str, &str, &str) -> anyhow::Result<()>;
type ClearLogin = fn(&dyn keel_vfs::cloud::SecretStore, &str) -> anyhow::Result<()>;

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
mod tests {
    /// Whatever `RUST_LOG` asks for, the FTP library's command log (`PASS <password>` at
    /// trace level, forwarded from `log`) never reaches the daemon log; Keel's own lines do.
    #[test]
    fn the_log_filter_never_lets_ftp_passwords_through() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for rust_log in [None, Some(""), Some("trace"), Some("suppaftp=trace,trace")] {
            let buf = Buf::default();
            let out = buf.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || out.clone())
                .with_env_filter(super::filter(rust_log))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                tracing::trace!(target: "suppaftp::sync_ftp", "CC OUT: PASS hunter2");
                tracing::error!(target: "suppaftp", "CC OUT: PASS hunter2");
                tracing::info!(target: "keel_daemon", "still logged");
            });
            let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
            assert!(!text.contains("PASS"), "{rust_log:?}: {text}");
            assert!(text.contains("still logged"), "{rust_log:?}: {text}");
        }
    }
    #[test]
    fn profile_names_are_validated() {
        use clap::Parser;
        for bad in ["../x", "a/b", "a\\b", "..", "", "nul"] {
            let args = crate::Args::try_parse_from(["keel-daemon", "--profile", bad]);
            assert!(args.is_err(), "{bad:?}");
        }
        let args = crate::Args::try_parse_from(["keel-daemon", "--profile", "work"]).unwrap();
        assert_eq!(args.profile, "work");
        // keel-app starts it with `--profile=<name>`: a leading '-' is no name.
        let args = crate::Args::try_parse_from(["keel-daemon", "--profile=work"]).unwrap();
        assert_eq!(args.profile, "work");
        let dashed = crate::Args::try_parse_from(["keel-daemon", "--profile=-x"]);
        assert!(dashed.is_err());
    }
}
