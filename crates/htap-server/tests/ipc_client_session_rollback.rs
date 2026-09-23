use std::sync::Arc;

use htap_server::ipc::{IpcClient, IpcRequest, ResponsePayload};
use htap_server::{LocalServer, Session, SessionId};
use htap_sql::result::StatementResult;

#[test]
fn remote_session_rollback_discards_buffered_insert() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let server = Arc::new(LocalServer::open(root.path()).expect("owner server opens"));
    server
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .expect("table creates");

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
    session.begin().expect("remote BEGIN succeeds");
    session
        .execute("INSERT INTO t (id, v) VALUES (1, 99)")
        .expect("remote INSERT succeeds");
    session.rollback().expect("remote ROLLBACK succeeds");

    let result = server
        .execute("SELECT id, v FROM t WHERE id = 1")
        .expect("query succeeds");
    let StatementResult::Query(query) = result else {
        panic!("expected query result, got {result:?}");
    };
    assert!(query.rows.is_empty(), "rolled-back row must not persist");
}
