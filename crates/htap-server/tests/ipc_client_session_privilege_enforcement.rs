use std::sync::Arc;

use htap_common::password::scramble_native_password;
use htap_common::HtapError;
use htap_server::LocalServer;

const SCRAMBLE: &[u8; 20] = b"01234567890123456789";

#[test]
fn ipc_client_session_enforces_account_privileges() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let owner = Arc::new(LocalServer::open(root.path()).expect("owner server opens"));
    owner
        .bootstrap_root_account(Some("root"))
        .expect("root account bootstraps");
    owner
        .execute("CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)")
        .expect("table creates");
    owner
        .execute("CREATE USER u IDENTIFIED BY 'pw'")
        .expect("account creates");
    owner
        .execute("GRANT SELECT ON t TO u")
        .expect("select grant succeeds");

    let response = scramble_native_password(SCRAMBLE, "pw");
    let mut local_session = owner
        .authenticate_session("u", SCRAMBLE, &response)
        .expect("local account session authenticates");
    let local_error = local_session
        .execute("INSERT INTO t (id, v) VALUES (1, 1)")
        .expect_err("local session lacks INSERT privilege");
    assert!(matches!(local_error, HtapError::PermissionDenied(_)));

    let client = Arc::new(LocalServer::open(root.path()).expect("client server opens"));
    assert!(!client.is_owner(), "second handle must use IPC forwarding");

    // Without forwarded authentication, the client session would run as superuser and wrongly succeed.
    let mut client_session = client
        .authenticate_session("u", SCRAMBLE, &response)
        .expect("remote account session authenticates");
    let client_error = client_session
        .execute("INSERT INTO t (id, v) VALUES (1, 1)")
        .expect_err("remote session lacks INSERT privilege");

    assert!(matches!(client_error, HtapError::PermissionDenied(_)));
    assert_eq!(client_error.to_string(), local_error.to_string());
}
