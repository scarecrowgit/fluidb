//! Subprocess lock helper for `LocalServer` contention tests.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::process::ExitCode;

use htap_common::HtapError;
use htap_server::LocalServer;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let dir = match args.next() {
        Some(d) => d,
        None => {
            eprintln!("usage: server_lock_child <dir> [hold|try_once]");
            return ExitCode::from(1);
        }
    };
    let mode = args.next().unwrap_or_else(|| "hold".to_string());

    match LocalServer::open(&dir) {
        Ok(_server) => {
            println!("LOCKED");
            let _ = std::io::stdout().flush();
            if mode == "hold" {
                let mut line = String::new();
                let stdin = std::io::stdin();
                let _ = stdin.lock().read_line(&mut line);
            }
            ExitCode::SUCCESS
        }
        Err(HtapError::Conflict(msg)) => {
            eprintln!("CONFLICT: {msg}");
            ExitCode::from(42)
        }
        Err(err) => {
            eprintln!("ERROR: {err}");
            ExitCode::from(2)
        }
    }
}
