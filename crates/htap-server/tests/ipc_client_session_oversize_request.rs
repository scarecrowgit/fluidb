#![cfg(unix)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use htap_common::error::HtapError;
use htap_server::ipc::{
    read_frame, write_frame, IpcClient, IpcRequest, IpcResponse, ResponsePayload, SessionStatus,
    IPC_PROTOCOL_VERSION, MAX_FRAME_SIZE,
};
use htap_server::{Principal, Session, SessionId};
use htap_sql::result::StatementResult;

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
fn oversized_request_is_rejected_locally_without_poisoning_session() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let listener = UnixListener::bind(root.path().join("htap.sock")).expect("listener binds");
    let (select_received, select_received_rx) = mpsc::channel();

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

        let (mut session_stream, _) = listener.accept().expect("session connection accepts");
        let _: IpcRequest = read_frame(&mut session_stream).expect("session handshake reads");
        send_response(&mut session_stream, ResponsePayload::Empty, status());

        let open: IpcRequest = read_frame(&mut session_stream).expect("open session reads");
        assert!(matches!(open, IpcRequest::OpenSession));
        send_response(
            &mut session_stream,
            ResponsePayload::SessionId(42),
            status(),
        );

        let select: IpcRequest = read_frame(&mut session_stream).expect("SELECT reads");
        assert!(matches!(select, IpcRequest::Execute { ref sql } if sql == "SELECT 1"));
        select_received.send(()).expect("SELECT receipt reports");
        send_response(
            &mut session_stream,
            ResponsePayload::StatementResult(StatementResult::query(Vec::new(), Vec::new())),
            status(),
        );
    });

    let client = IpcClient::connect(root.path()).expect("client connects");
    let mut connection = client
        .open_connection()
        .expect("persistent connection opens");
    let (status, payload) = connection
        .request(IpcRequest::OpenSession)
        .expect("remote session opens");
    let ResponsePayload::SessionId(session_id) = payload.expect("OPEN SESSION succeeds") else {
        panic!("OPEN SESSION returned an unexpected response");
    };

    let mut session = Session::open_remote(SessionId(session_id), connection, status);
    let error = session
        .execute(&"x".repeat(MAX_FRAME_SIZE + 1))
        .expect_err("oversized SQL is rejected locally");
    assert!(matches!(error, HtapError::InvalidArgument(_)));

    session
        .execute("SELECT 1")
        .expect("session remains usable after local rejection");
    select_received_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake owner received SELECT");

    server.join().expect("fake owner exits");
}
