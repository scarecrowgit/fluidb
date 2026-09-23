#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use htap_common::types::Value;
use htap_server::ipc::{
    read_frame, write_frame, IpcRequest, IpcResponse, ResponsePayload, IPC_PROTOCOL_VERSION,
};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

fn request(stream: &mut UnixStream, request: IpcRequest) -> IpcResponse {
    write_frame(stream, &request).expect("request writes");
    read_frame(stream).expect("response reads")
}

#[test]
fn transaction_commit_is_visible_afterwards() {
    let root = tempfile::tempdir().expect("temporary root");
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));
    server
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .expect("table creates");

    let mut stream = UnixStream::connect(root.path().join("htap.sock")).expect("connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout sets");

    let canonical_root = root.path().canonicalize().expect("root canonicalizes");
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

    let open_session = request(&mut stream, IpcRequest::OpenSession);
    assert!(
        matches!(open_session.result, Ok(ResponsePayload::SessionId(_))),
        "open session failed: {open_session:?}"
    );
    let session_id = match open_session.result {
        Ok(ResponsePayload::SessionId(session_id)) => session_id,
        _ => unreachable!("response was asserted above"),
    };

    let begin = request(&mut stream, IpcRequest::Begin { session_id });
    assert!(
        matches!(begin.result, Ok(ResponsePayload::Empty)),
        "begin failed: {begin:?}"
    );
    assert!(
        begin.status.in_transaction,
        "begin did not leave the session in a transaction: {begin:?}"
    );

    let insert = request(
        &mut stream,
        IpcRequest::Execute {
            sql: "INSERT INTO t (id, v) VALUES (1, 9)".into(),
        },
    );
    assert!(
        matches!(insert.result, Ok(ResponsePayload::StatementResult(_))),
        "insert failed: {insert:?}"
    );
    assert!(
        insert.status.in_transaction,
        "insert unexpectedly ended the transaction: {insert:?}"
    );

    let commit = request(&mut stream, IpcRequest::Commit { session_id });
    assert!(
        matches!(commit.result, Ok(ResponsePayload::Empty)),
        "commit failed: {commit:?}"
    );
    assert!(
        !commit.status.in_transaction,
        "commit left the session in a transaction: {commit:?}"
    );

    let result = server
        .execute("SELECT id, v FROM t WHERE id = 1")
        .expect("committed row reads");
    let StatementResult::Query(query) = result else {
        panic!("expected query result after commit, got {result:?}");
    };
    assert_eq!(
        query.rows.len(),
        1,
        "expected exactly one committed row for id 1, got {:?}",
        query.rows
    );
    assert_eq!(
        query.rows[0].values(),
        &[Value::Int32(1), Value::Int32(9)],
        "committed row values differ"
    );
}
