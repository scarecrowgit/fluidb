use std::sync::Arc;

use htap_common::types::Value;
use htap_server::ipc::{IpcClient, IpcRequest, ResponsePayload};
use htap_server::{LocalServer, Session, SessionId};
use htap_sql::result::StatementResult;

#[test]
fn remote_session_plain_select_reads_preexisting_data() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let server = Arc::new(LocalServer::open(root.path()).expect("owner server opens"));
    server
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .expect("table creates");
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 99), (2, 42)")
        .expect("test data inserts");

    let client = IpcClient::connect(root.path()).expect("client connects");
    let mut connection = client
        .open_connection()
        .expect("persistent connection opens");
    let (status, payload) = connection
        .request(IpcRequest::OpenSession)
        .expect("remote session opens");
    let ResponsePayload::SessionId(session_id) = payload.expect("OPEN SESSION response succeeds")
    else {
        panic!("OPEN SESSION returned an unexpected response");
    };

    let mut session = Session::open_remote(SessionId(session_id), connection, status);
    let result = session
        .execute("SELECT id, v FROM t ORDER BY id")
        .expect("remote SELECT succeeds");
    let StatementResult::Query(query) = result else {
        panic!("expected query result, got {result:?}");
    };

    assert_eq!(query.rows.len(), 2);
    assert_eq!(query.rows[0].values(), &[Value::Int32(1), Value::Int32(99)]);
    assert_eq!(query.rows[1].values(), &[Value::Int32(2), Value::Int32(42)]);
}
