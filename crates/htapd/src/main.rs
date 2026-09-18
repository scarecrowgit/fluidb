//! `htapd`: network daemon exposing a local fluidb root over the MySQL text protocol.
//!
//! ```text
//! htapd --root <dir> [--listen 127.0.0.1:3307] [--max-connections 64] [--password <pw>]
//!       [--max-allowed-packet 67108864] [--tls-cert <path> --tls-key <path>]
//!       [--require-secure-transport]
//! ```
//!
//! The password may also come from `HTAPD_PASSWORD`, and the packet size limit from
//! `HTAPD_MAX_ALLOWED_PACKET`; in both cases the flag wins. With neither password source set, no
//! password is required. The default bind address is loopback only. TLS is available with
//! `--tls-cert` and `--tls-key`; without TLS, query text and results are cleartext on any
//! non-loopback address.
//!
//! The daemon runs until the process is killed (Ctrl-C / SIGTERM). Storage is crash-safe by
//! design, so an abrupt stop is recovered on the next start.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use htap_server::LocalServer;
use htap_wire::{TlsConfig, WireServer, WireServerConfig};

const USAGE: &str = "\
htapd - fluidb network daemon (MySQL text protocol)

USAGE:
    htapd --root <dir> [OPTIONS]

OPTIONS:
    --root <dir>              Data directory (created if missing). Required.
    --listen <addr>           Bind address. Default 127.0.0.1:3307 (loopback only).
    --max-connections <n>     Maximum simultaneous connections. Default 64.
    --password <pw>           Shared password (mysql_native_password). Overrides HTAPD_PASSWORD.
    --tls-cert <path>         PEM TLS certificate. Requires --tls-key. Overrides HTAPD_TLS_CERT.
    --tls-key <path>          PEM TLS private key. Requires --tls-cert. Overrides HTAPD_TLS_KEY.
    --require-secure-transport
                              Reject non-TLS connections. Overrides HTAPD_REQUIRE_SECURE_TRANSPORT.
    --disable-compression     Disable MySQL protocol compression. Overrides HTAPD_DISABLE_COMPRESSION.
    --max-allowed-packet <n>  Maximum protocol message size, in bytes. Default 64 MiB. Overrides
                              HTAPD_MAX_ALLOWED_PACKET.
    --help                    Print this help.

SECURITY:
    TLS is available with --tls-cert and --tls-key; --require-secure-transport rejects
    non-TLS connections. Without TLS, binding a non-loopback address exposes query text and
    results in cleartext; only the password exchange is hashed. Use TLS, a trusted network,
    or an SSH tunnel.

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
    if let Ok(v) = std::env::var("HTAPD_MAX_ALLOWED_PACKET") {
        if !v.is_empty() {
            config.max_allowed_packet = v
                .parse::<usize>()
                .map_err(|e| format!("invalid HTAPD_MAX_ALLOWED_PACKET '{v}': {e}"))?;
        }
    }

    let env_tls_cert = std::env::var("HTAPD_TLS_CERT")
        .ok()
        .filter(|path| !path.is_empty());
    let env_tls_key = std::env::var("HTAPD_TLS_KEY")
        .ok()
        .filter(|path| !path.is_empty());
    if env_tls_cert.is_some() || env_tls_key.is_some() {
        let cert_path =
            env_tls_cert.ok_or("HTAPD_TLS_CERT and HTAPD_TLS_KEY must be set together")?;
        let key_path =
            env_tls_key.ok_or("HTAPD_TLS_CERT and HTAPD_TLS_KEY must be set together")?;
        config.tls = Some(TlsConfig {
            cert_path: PathBuf::from(cert_path),
            key_path: PathBuf::from(key_path),
        });
    }
    if let Ok(v) = std::env::var("HTAPD_REQUIRE_SECURE_TRANSPORT") {
        if !v.is_empty() {
            config.require_secure_transport = match v.as_str() {
                "1" | "true" | "TRUE" | "yes" | "YES" => true,
                "0" | "false" | "FALSE" | "no" | "NO" => false,
                _ => {
                    return Err(format!(
                        "invalid HTAPD_REQUIRE_SECURE_TRANSPORT '{v}': expected true or false"
                    ))
                }
            };
        }
    }
    if let Ok(v) = std::env::var("HTAPD_DISABLE_COMPRESSION") {
        if !v.is_empty() {
            let disable = match v.as_str() {
                "1" | "true" | "TRUE" | "yes" | "YES" => true,
                "0" | "false" | "FALSE" | "no" | "NO" => false,
                _ => {
                    return Err(format!(
                        "invalid HTAPD_DISABLE_COMPRESSION '{v}': expected true or false"
                    ))
                }
            };
            if disable {
                config.compression_enabled = false;
            }
        }
    }

    let mut tls_cert_flag = None;
    let mut tls_key_flag = None;
    let mut require_secure_transport_flag = None;
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
            "--tls-cert" => {
                tls_cert_flag = Some(args.next().ok_or("--tls-cert requires a value")?);
            }
            "--tls-key" => {
                tls_key_flag = Some(args.next().ok_or("--tls-key requires a value")?);
            }
            "--require-secure-transport" => {
                require_secure_transport_flag = Some(true);
            }
            "--disable-compression" => {
                config.compression_enabled = false;
            }
            "--max-allowed-packet" => {
                let v = args.next().ok_or("--max-allowed-packet requires a value")?;
                config.max_allowed_packet = v
                    .parse::<usize>()
                    .map_err(|e| format!("invalid --max-allowed-packet '{v}': {e}"))?;
                if config.max_allowed_packet == 0 {
                    return Err("--max-allowed-packet must be at least 1".into());
                }
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if tls_cert_flag.is_some() || tls_key_flag.is_some() {
        let cert_path = tls_cert_flag.ok_or("--tls-cert and --tls-key must be set together")?;
        let key_path = tls_key_flag.ok_or("--tls-cert and --tls-key must be set together")?;
        config.tls = Some(TlsConfig {
            cert_path: PathBuf::from(cert_path),
            key_path: PathBuf::from(key_path),
        });
    }
    if let Some(value) = require_secure_transport_flag {
        config.require_secure_transport = value;
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
