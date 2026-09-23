use std::sync::Arc;

use htap_server::ipc::{IpcClient, IpcRequest, ResponsePayload};
use htap_server::{LocalServer, Session, SessionId};

#[test]
fn remote_session_forwards_statement_visibility_check() {
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
    let statement = htap_sql::parse_one("SELECT * FROM t").expect("statement parses");

    // Before this fix, the owner discarded this request's catalog snapshot and replied with an
    // empty payload, so every prepared statement failed in client mode with an internal error;
    // no test covered this path.
    let catalog = session
        .check_statement_visible(&statement)
        .expect("visibility check forwards to owner");

    assert!(
        catalog.table_by_name("t").is_some(),
        "forwarded visibility check must return metadata for table t"
    );
}
