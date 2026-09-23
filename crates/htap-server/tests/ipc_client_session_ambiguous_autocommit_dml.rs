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
fn remote_session_preserves_ambiguous_autocommit_dml_outcome() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let listener = UnixListener::bind(root.path().join("htap.sock")).expect("listener binds");
    let (execute_received, execute_received_rx) = mpsc::channel();

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

        // Ambiguity is not specific to commit: any statement that may already have been
        // applied by the owner leaves the same unknown outcome.
        let execute: IpcRequest =
            read_frame(&mut session_stream).expect("autocommit INSERT reads completely");
        assert!(matches!(execute, IpcRequest::Execute { .. }));
        execute_received
            .send(())
            .expect("statement receipt reports");
        // Drop without a response: the fully sent statement has an ambiguous outcome.
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
        .execute("INSERT INTO t (id, v) VALUES (1, 99)")
        .expect_err("response-less autocommit INSERT is ambiguous");
    assert!(matches!(error, HtapError::Ambiguous(_)));
    let error_message = error.to_string();

    execute_received_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake owner received complete INSERT");

    let rollback_error = session
        .rollback()
        .expect_err("ambiguous state rejects rollback");
    assert!(matches!(rollback_error, HtapError::Ambiguous(_)));
    assert_eq!(rollback_error.to_string(), error_message);

    let execute_error = session
        .execute("SELECT 1")
        .expect_err("ambiguous state rejects further statements");
    assert!(matches!(execute_error, HtapError::Ambiguous(_)));
    assert_eq!(execute_error.to_string(), error_message);

    let begin_error = session
        .begin()
        .expect_err("ambiguous state rejects further transaction control");
    assert!(matches!(begin_error, HtapError::Ambiguous(_)));
    assert_eq!(begin_error.to_string(), error_message);

    let commit_error = session
        .commit()
        .expect_err("ambiguous state rejects commit");
    assert!(matches!(commit_error, HtapError::Ambiguous(_)));
    assert_eq!(commit_error.to_string(), error_message);

    server.join().expect("fake owner exits");
}
