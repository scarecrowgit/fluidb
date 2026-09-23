#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use htap_server::ipc::{
    read_frame, write_frame, IpcRequest, IpcResponse, ResponsePayload, IPC_PROTOCOL_VERSION,
};
use htap_server::LocalServer;
use htap_sql::result::StatementResult;

fn request(stream: &mut UnixStream, request: IpcRequest) -> IpcResponse {
    write_frame(stream, &request).unwrap();
    read_frame(stream).unwrap()
}

#[test]
fn disconnect_rolls_back_open_transaction() {
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(LocalServer::open(root.path()).unwrap());
    server
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .unwrap();

    let mut stream = UnixStream::connect(root.path().join("htap.sock")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    request(
        &mut stream,
        IpcRequest::Handshake {
            protocol_version: IPC_PROTOCOL_VERSION,
            canonical_root: canonical_root.to_string_lossy().into_owned(),
        },
    );
    let Ok(ResponsePayload::SessionId(session_id)) =
        request(&mut stream, IpcRequest::OpenSession).result
    else {
        panic!("session opens");
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

    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let StatementResult::Query(query) = server.execute("SELECT * FROM t").unwrap() else {
            panic!("query result expected");
        };
        if query.rows.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "transaction was not rolled back after disconnect; rows: {:?}",
            query.rows
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
