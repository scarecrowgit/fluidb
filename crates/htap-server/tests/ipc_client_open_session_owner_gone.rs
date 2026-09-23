#![cfg(unix)]

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use htap_server::LocalServer;

fn assert_errors_within(operation: impl FnOnce() -> bool + Send + 'static) {
    let (sender, receiver) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = sender.send(operation());
    });

    match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(true) => handle
            .join()
            .expect("client operation thread must not panic"),
        Ok(false) => panic!("client operation unexpectedly succeeded"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("client operation did not return within one second")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            handle
                .join()
                .expect("client operation thread must not panic");
            panic!("client operation thread disconnected without returning a result");
        }
    }
}

#[test]
fn ipc_client_session_operations_error_when_owner_has_gone_away() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let owner = Arc::new(LocalServer::open(root.path()).expect("owner server opens"));
    let client = Arc::new(LocalServer::open(root.path()).expect("client server opens"));

    assert!(!client.is_owner(), "second handle must use IPC forwarding");

    drop(owner);

    // Both paths used to panic, crashing the wire server connection thread and potentially
    // aborting the process when configured with abort-on-panic.
    let open_session_client = Arc::clone(&client);
    assert_errors_within(move || open_session_client.open_session().is_err());

    let authenticate_client = Arc::clone(&client);
    assert_errors_within(move || {
        authenticate_client
            .authenticate_session("missing", b"arbitrary-scramble", b"arbitrary-response")
            .is_err()
    });
}
