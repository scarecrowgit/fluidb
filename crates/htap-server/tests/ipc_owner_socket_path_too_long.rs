#![cfg(unix)]

use std::sync::Arc;

use htap_server::LocalServer;

#[test]
fn too_long_socket_path_keeps_owner_in_lock_only_mode() {
    let base = tempfile::tempdir().unwrap();
    let component = "a".repeat(80);
    let root = base
        .path()
        .join(&component)
        .join(&component)
        .join(&component)
        .join(&component);
    let server = Arc::new(LocalServer::open(&root).unwrap());

    assert!(server.is_owner());
    assert!(!server.is_listener_up());
}
