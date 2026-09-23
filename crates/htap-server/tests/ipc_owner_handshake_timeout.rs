#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use htap_server::ipc::{read_frame, write_frame, IpcRequest, IpcResponse, IPC_PROTOCOL_VERSION};
use htap_server::LocalServer;

#[test]
fn silent_handshake_times_out_and_releases_connection_slot() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));
    let socket_path = root.path().join("htap.sock");

    let mut silent_stream = UnixStream::connect(&socket_path).expect("silent client connects");
    silent_stream
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("timeout sets");

    // Without the handshake deadline, this client would occupy one of the fixed connection slots
    // permanently, and enough silent clients would prevent the owner serving new clients.
    assert!(read_frame::<_, IpcResponse>(&mut silent_stream).is_err());

    let mut stream = UnixStream::connect(&socket_path).expect("later client connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout sets");

    write_frame(
        &mut stream,
        &IpcRequest::Handshake {
            protocol_version: IPC_PROTOCOL_VERSION,
            canonical_root: root
                .path()
                .canonicalize()
                .expect("root canonicalizes")
                .to_string_lossy()
                .into_owned(),
        },
    )
    .expect("handshake writes");

    let response: IpcResponse = read_frame(&mut stream).expect("handshake response");
    assert!(response.result.is_ok());

    write_frame(
        &mut stream,
        &IpcRequest::Execute {
            sql: "SELECT 1".into(),
        },
    )
    .expect("execute writes");

    let response: IpcResponse = read_frame(&mut stream).expect("execute response");
    assert!(response.result.is_ok());

    drop(server);
}
