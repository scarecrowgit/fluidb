use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use htap_server::LocalServer;

fn test_root(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "htap-server-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos()
    ))
}

#[test]
fn test_open_owner() {
    let root = test_root("open-owner");
    let server = LocalServer::open(&root).expect("server opens");

    assert!(server.is_owner());

    drop(server);
    std::fs::remove_dir_all(root).expect("test server directory is removed");
}

#[test]
fn test_concurrent_open_conflict() {
    let root = test_root("concurrent-open");
    let server = LocalServer::open(&root).expect("first server opens");
    let client = LocalServer::open(&root).expect("second server connects as a client");

    assert!(server.is_owner());
    assert!(!client.is_owner());

    // Verify the client can execute queries through IPC.
    client.execute("SELECT 1").expect("client query succeeds");

    drop(client);
    drop(server);
    std::fs::remove_dir_all(root).expect("test server directory is removed");
}

#[test]
fn test_session_ids_distinct() {
    let root = test_root("session-ids");
    let server = Arc::new(LocalServer::open(&root).expect("server opens"));

    assert!(server.is_owner());

    let first = server.open_session().unwrap();
    let second = server.open_session().unwrap();

    assert_ne!(first.id(), second.id());

    drop(first);
    drop(second);
    drop(server);
    std::fs::remove_dir_all(root).expect("test server directory is removed");
}
