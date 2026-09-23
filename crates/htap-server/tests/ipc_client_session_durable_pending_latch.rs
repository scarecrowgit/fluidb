#![cfg(unix)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use htap_common::error::HtapError;
use htap_common::Version;
use htap_server::ipc::{
    read_frame, write_frame, IpcClient, IpcRequest, IpcResponse, ResponsePayload, SessionStatus,
    WireError, IPC_PROTOCOL_VERSION,
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

fn send_error(stream: &mut UnixStream, error: WireError, status: SessionStatus) {
    write_frame(
        stream,
        &IpcResponse {
            status,
            result: Err(error),
        },
    )
    .expect("error response writes");
}

fn assert_durable_pending(error: HtapError) {
    assert!(matches!(
        error,
        HtapError::DurablePending {
            txn_id: 42,
            version,
            reason,
        } if version == Version::new(100) && reason == "apply failed after durable commit"
    ));
}

#[test]
fn remote_session_latches_durable_pending_commit_outcome() {
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

        let commit: IpcRequest = read_frame(&mut session_stream).expect("COMMIT reads");
        assert!(matches!(commit, IpcRequest::Commit { session_id: 42 }));
        commit_received.send(()).expect("commit receipt reports");
        send_error(
            &mut session_stream,
            WireError::DurablePending {
                txn_id: 42,
                version: Version::new(100),
                reason: "apply failed after durable commit".into(),
            },
            SessionStatus {
                in_transaction: true,
                ..status()
            },
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
    session.begin().expect("BEGIN succeeds");
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 99)")
        .expect("INSERT succeeds");

    assert_durable_pending(session.commit().expect_err("COMMIT is durable pending"));
    commit_received_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fake owner received COMMIT");

    assert_durable_pending(
        session
            .rollback()
            .expect_err("durable-pending latch rejects rollback"),
    );
    assert_durable_pending(
        session
            .execute("SELECT 1")
            .expect_err("durable-pending latch rejects statements"),
    );

    server.join().expect("fake owner exits");
}
