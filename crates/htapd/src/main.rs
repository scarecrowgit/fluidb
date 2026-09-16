//! `htapd`: network daemon exposing a local fluidb root over the MySQL text protocol.
//!
//! ```text
//! htapd --root <dir> [--listen 127.0.0.1:3307] [--max-connections 64] [--password <pw>]
//! ```
//!
//! The password may also come from `HTAPD_PASSWORD`; the flag wins. With neither set, no
//! password is required. The default bind address is loopback only; there is no TLS, so
//! query text and results are cleartext on any non-loopback address.
//!
//! The daemon runs until the process is killed (Ctrl-C / SIGTERM). Storage is crash-safe by
//! design, so an abrupt stop is recovered on the next start.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use htap_server::LocalServer;
use htap_wire::{WireServer, WireServerConfig};

const USAGE: &str = "\
htapd - fluidb network daemon (MySQL text protocol)

USAGE:
    htapd --root <dir> [OPTIONS]

OPTIONS:
    --root <dir>              Data directory (created if missing). Required.
    --listen <addr>           Bind address. Default 127.0.0.1:3307 (loopback only).
    --max-connections <n>     Maximum simultaneous connections. Default 64.
    --password <pw>           Shared password (mysql_native_password). Overrides HTAPD_PASSWORD.
    --help                    Print this help.

SECURITY:
    No TLS. Binding a non-loopback address exposes query text and results in cleartext;
    only the password exchange is hashed. Use a trusted network or an SSH tunnel.

SHUTDOWN:
    Stop the process with Ctrl-C or SIGTERM. In-flight statements are not drained; the
    storage layer recovers committed state on the next start.
";

struct Args {
    root: String,
    config: WireServerConfig,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut root = None;
    let mut config = WireServerConfig::default();
    if let Ok(pw) = std::env::var("HTAPD_PASSWORD") {
        if !pw.is_empty() {
            config.password = Some(pw);
        }
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--root" => root = Some(args.next().ok_or("--root requires a value")?),
            "--listen" => {
                let v = args.next().ok_or("--listen requires a value")?;
                config.listen = v
                    .parse::<SocketAddr>()
                    .map_err(|e| format!("invalid --listen address '{v}': {e}"))?;
            }
            "--max-connections" => {
                let v = args.next().ok_or("--max-connections requires a value")?;
                config.max_connections = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --max-connections '{v}': {e}"))?;
                if config.max_connections == 0 {
                    return Err("--max-connections must be at least 1".into());
                }
            }
            "--password" => {
                config.password = Some(args.next().ok_or("--password requires a value")?);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    let root = root.ok_or("--root is required")?;
    Ok(Some(Args { root, config }))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let server = match LocalServer::open(&args.root) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("error: failed to open root '{}': {e}", args.root);
            return ExitCode::from(1);
        }
    };
    let wire = match WireServer::start(args.config.clone(), server) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("error: failed to bind {}: {e}", args.config.listen);
            return ExitCode::from(1);
        }
    };
    tracing::info!(
        listen = %wire.local_addr(),
        root = %args.root,
        password_required = args.config.password.is_some(),
        "htapd ready"
    );
    if !wire.local_addr().ip().is_loopback() {
        tracing::warn!("listening on a non-loopback address without TLS: traffic is cleartext");
    }
    loop {
        std::thread::park();
    }
}
