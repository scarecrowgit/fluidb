#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use htap_server::ipc::{
    read_frame, write_frame, IpcRequest, IpcResponse, ResponsePayload, IPC_PROTOCOL_VERSION,
};
use htap_server::LocalServer;

fn request(stream: &mut UnixStream, request: IpcRequest) -> IpcResponse {
    write_frame(stream, &request).unwrap();
    read_frame(stream).unwrap()
}

#[test]
fn dropping_owner_with_idle_client_is_bounded_and_releases_lock() {
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(LocalServer::open(root.path()).unwrap());
    let mut stream = UnixStream::connect(root.path().join("htap.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    let canonical_root = root.path().canonicalize().unwrap();
    let handshake = request(
        &mut stream,
        IpcRequest::Handshake {
            protocol_version: IPC_PROTOCOL_VERSION,
            canonical_root: canonical_root.to_string_lossy().into_owned(),
        },
    );
    assert!(
        matches!(handshake.result, Ok(ResponsePayload::Empty)),
        "handshake failed: {handshake:?}"
    );

    let (done_tx, done_rx) = mpsc::channel();
    let root_path = root.path().to_path_buf();
    std::thread::spawn(move || {
        drop(server);
        let reopened = LocalServer::open(root_path);
        let reopened_as_owner = reopened.as_ref().is_ok_and(|server| server.is_owner());
        done_tx.send(reopened_as_owner).unwrap();
    });

    assert!(done_rx.recv_timeout(Duration::from_secs(3)).unwrap());
    drop(stream);
}
