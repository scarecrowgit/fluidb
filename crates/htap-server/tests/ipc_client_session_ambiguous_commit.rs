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
fn remote_session_preserves_ambiguous_commit_outcome() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let listener = UnixListener::bind(root.path().join("htap.sock")).expect("listener binds");
    let (commit_received, commit_received_rx) = mpsc::channel();

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

        let begin: IpcRequest = read_frame(&mut session_stream).expect("BEGIN reads");
        assert!(matches!(begin, IpcRequest::Begin { session_id: 42 }));
        send_response(
            &mut session_stream,
            ResponsePayload::Empty,
            SessionStatus {
                in_transaction: true,
                ..status()
            },
        );

        let execute: IpcRequest = read_frame(&mut session_stream).expect("INSERT reads");
        assert!(matches!(execute, IpcRequest::Execute { .. }));
        send_response(
            &mut session_stream,
            ResponsePayload::StatementResult(StatementResult::ddl(1)),
            SessionStatus {
                in_transaction: true,
                ..status()
            },
        );

        let commit: IpcRequest = read_frame(&mut session_stream).expect("COMMIT reads completely");
        assert!(matches!(commit, IpcRequest::Commit { session_id: 42 }));
        commit_received.send(()).expect("commit receipt reports");
        // Drop without a response: the fully sent COMMIT has an ambiguous outcome.
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
    session.begin().expect("BEGIN succeeds");
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 99)")
        .expect("INSERT succeeds");

    let error = session
        .commit()
        .expect_err("response-less COMMIT is ambiguous");
    assert!(matches!(error, HtapError::Ambiguous(_)));
    commit_received_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake owner received complete COMMIT");

    let rollback_error = session
        .rollback()
        .expect_err("ambiguous state rejects rollback");
    assert!(matches!(rollback_error, HtapError::Ambiguous(_)));

    let execute_error = session
        .execute("SELECT 1")
        .expect_err("ambiguous state rejects further statements");
    assert!(matches!(execute_error, HtapError::Ambiguous(_)));

    server.join().expect("fake owner exits");
}
