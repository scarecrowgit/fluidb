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
use htap_server::{Principal, Session, SessionId};

fn status() -> SessionStatus {
    SessionStatus {
        autocommit: true,
        in_transaction: false,
        principal: Principal::Superuser,
    }
}

fn send_response(stream: &mut UnixStream, payload: ResponsePayload) {
    write_frame(
        stream,
        &IpcResponse {
            status: status(),
            result: Ok(payload),
        },
    )
    .expect("response writes");
}

#[test]
fn remote_session_preserves_disconnect_before_request_outcome() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let listener = UnixListener::bind(root.path().join("htap.sock")).expect("listener binds");
    let (session_closed, session_closed_rx) = mpsc::channel();

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
        send_response(&mut connect_stream, ResponsePayload::Empty);

        let (mut session_stream, _) = listener.accept().expect("session connection accepts");
        let _: IpcRequest = read_frame(&mut session_stream).expect("session handshake reads");
        send_response(&mut session_stream, ResponsePayload::Empty);

        let open: IpcRequest = read_frame(&mut session_stream).expect("open session reads");
        assert!(matches!(open, IpcRequest::OpenSession));
        send_response(&mut session_stream, ResponsePayload::SessionId(42));

        drop(session_stream);
        session_closed.send(()).expect("session closure reports");
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
    session_closed_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake owner closes the session connection");
    server.join().expect("fake owner exits");

    // Treating this as ambiguous would wrongly imply a write might have been applied even
    // though the owner closed the connection before any request bytes left the client.
    let error = session
        .execute("INSERT INTO t (id, v) VALUES (1, 99)")
        .expect_err("disconnected session rejects the unsent request");
    let HtapError::Conflict(reason) = error else {
        panic!("disconnect before request must not be ambiguous");
    };

    let next_error = session
        .execute("SELECT 1")
        .expect_err("terminal disconnected session rejects later requests");
    let HtapError::Conflict(next_reason) = next_error else {
        panic!("terminal disconnected session must keep reporting a safe disconnect");
    };
    assert_eq!(next_reason, reason);
}
