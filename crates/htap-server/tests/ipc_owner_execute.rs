#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use htap_server::ipc::{
    read_frame, write_frame, IpcRequest, IpcResponse, ResponsePayload, IPC_PROTOCOL_VERSION,
};
use htap_server::LocalServer;

#[test]
fn execute_matches_in_process_result() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));
    let canonical_root = root.path().canonicalize().expect("root canonicalizes");
    let expected = server.execute("SELECT 1 + 2").expect("in-process query");
    let mut stream = UnixStream::connect(root.path().join("htap.sock")).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    write_frame(
        &mut stream,
        &IpcRequest::Handshake {
            protocol_version: IPC_PROTOCOL_VERSION,
            canonical_root: canonical_root.to_string_lossy().into_owned(),
        },
    )
    .unwrap();
    let _: IpcResponse = read_frame(&mut stream).unwrap();

    write_frame(
        &mut stream,
        &IpcRequest::Execute {
            sql: "SELECT 1 + 2".into(),
        },
    )
    .unwrap();
    let response: IpcResponse = read_frame(&mut stream).unwrap();
    assert_eq!(
        response.result,
        Ok(ResponsePayload::StatementResult(expected))
    );
}
