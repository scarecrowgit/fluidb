//! Subprocess IPC owner helper for `LocalServer` multiprocess tests.

#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::process::ExitCode;

use htap_server::LocalServer;

fn main() -> ExitCode {
    let dir = match std::env::args().nth(1) {
        Some(dir) => dir,
        None => {
            eprintln!("usage: server_ipc_child <dir>");
            return ExitCode::from(1);
        }
    };

    match LocalServer::open(&dir) {
        Ok(server) if server.is_owner() => {
            println!("IPC_OWNER_READY");
            let _ = std::io::stdout().flush();

            let mut line = String::new();
            let stdin = std::io::stdin();
            let _ = stdin.lock().read_line(&mut line);
            ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!("ERROR: child did not become the IPC owner");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::from(2)
        }
    }
}
