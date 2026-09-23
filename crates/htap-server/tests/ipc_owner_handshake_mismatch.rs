#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use htap_server::ipc::{
    read_frame, write_frame, IpcRequest, IpcResponse, WireError, IPC_PROTOCOL_VERSION,
};
use htap_server::LocalServer;

#[test]
fn handshake_mismatch_returns_error_then_closes() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));
    let mut stream = UnixStream::connect(root.path().join("htap.sock")).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout sets");

    write_frame(
        &mut stream,
        &IpcRequest::Handshake {
            protocol_version: IPC_PROTOCOL_VERSION + 1,
            canonical_root: root
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        },
    )
    .expect("handshake writes");

    let response: IpcResponse = read_frame(&mut stream).expect("error response");
    assert!(matches!(
        response.result,
        Err(WireError::InvalidArgument(_))
    ));
    assert!(read_frame::<_, IpcResponse>(&mut stream).is_err());

    drop(server);
}
