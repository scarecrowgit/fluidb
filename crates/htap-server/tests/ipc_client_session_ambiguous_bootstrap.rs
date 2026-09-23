#![cfg(unix)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use htap_common::error::HtapError;
use htap_server::ipc::{
    read_frame, write_frame, IpcClient, IpcRequest, IpcResponse, ResponsePayload, SessionStatus,
    IPC_PROTOCOL_VERSION,
};
use htap_server::Principal;

fn status() -> SessionStatus {
    SessionStatus {
        autocommit: true,
        in_transaction: false,
        principal: Principal::Superuser,
    }
}

fn send_response(stream: &mut UnixStream, payload: ResponsePayload, status: SessionStatus) {
    write_frame(
        stream,
        &IpcResponse {
            status,
            result: Ok(payload),
        },
    )
    .expect("response writes");
}

#[test]
fn bootstrap_root_account_preserves_ambiguous_outcome() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let listener = UnixListener::bind(root.path().join("htap.sock")).expect("listener binds");
    let (bootstrap_received, bootstrap_received_rx) = mpsc::channel();

    let server = thread::spawn(move || {
        let (mut connect_stream, _) = listener.accept().expect("connect handshake accepts");
        let handshake: IpcRequest = read_frame(&mut connect_stream).expect("handshake reads");
        assert!(matches!(
            handshake,
            IpcRequest::Handshake {
                protocol_version: IPC_PROTOCOL_VERSION,
                ..
            }
        ));
        send_response(&mut connect_stream, ResponsePayload::Empty, status());

        let (mut bootstrap_stream, _) = listener.accept().expect("bootstrap connection accepts");
        let _: IpcRequest = read_frame(&mut bootstrap_stream).expect("bootstrap handshake reads");
        send_response(&mut bootstrap_stream, ResponsePayload::Empty, status());

        let bootstrap: IpcRequest =
            read_frame(&mut bootstrap_stream).expect("bootstrap request reads completely");
        assert!(matches!(bootstrap, IpcRequest::BootstrapRootAccount { .. }));
        bootstrap_received
            .send(())
            .expect("bootstrap receipt reports");
        // Drop without a response: the fully sent bootstrap has an ambiguous outcome.
    });

    let client = IpcClient::connect(root.path()).expect("client connects");

    // Classifying this as a plain conflict would wrongly tell a caller the bootstrap definitely
    // did not happen, when the owner may already have created the account.
    let error = client
        .bootstrap_root_account(Some("secret"))
        .expect_err("response-less bootstrap is ambiguous");
    assert!(matches!(error, HtapError::Ambiguous(_)));
    bootstrap_received_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake owner received complete bootstrap");

    server.join().expect("fake owner exits");
}
