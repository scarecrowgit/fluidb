#![cfg(unix)]

use std::sync::Arc;

use htap_server::LocalServer;

#[test]
fn regular_socket_path_file_keeps_owner_in_lock_only_mode() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("htap.sock"), b"not a socket").unwrap();

    let server = Arc::new(LocalServer::open(root.path()).unwrap());

    assert!(server.is_owner());
    assert!(!server.is_listener_up());
    assert_eq!(
        std::fs::read(root.path().join("htap.sock")).unwrap(),
        b"not a socket"
    );
}
