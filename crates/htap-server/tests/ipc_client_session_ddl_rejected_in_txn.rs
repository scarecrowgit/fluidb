use std::sync::Arc;

use htap_server::ipc::{IpcClient, IpcRequest, ResponsePayload};
use htap_server::{LocalServer, Session, SessionId};

#[test]
fn remote_session_ddl_in_transaction_matches_local_rejection() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let server = Arc::new(LocalServer::open(root.path()).expect("owner server opens"));
    let client_handle = LocalServer::open(root.path()).expect("client server opens");
    assert!(!client_handle.is_owner());

    let mut local_session = server.open_session().unwrap();
    local_session.begin().expect("local BEGIN succeeds");
    let local_error = local_session
        .execute("CREATE TABLE local_t (id INT NOT NULL PRIMARY KEY)")
        .expect_err("local DDL in transaction rejects")
        .to_string();

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
    session.begin().expect("remote BEGIN succeeds");
    let remote_error = session
        .execute("CREATE TABLE remote_t (id INT NOT NULL PRIMARY KEY)")
        .expect_err("remote DDL in transaction rejects")
        .to_string();

    assert_eq!(remote_error, local_error);
}
