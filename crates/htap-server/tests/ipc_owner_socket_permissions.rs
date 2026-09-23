#![cfg(unix)]

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::sync::Arc;

use htap_server::LocalServer;

#[test]
fn startup_publishes_owner_only_socket_and_cleans_up_staging_directory() {
    let root = tempfile::tempdir().expect("temporary root");
    let socket_path = root.path().join("htap.sock");

    // The previous unit test could not fail if this fix were reverted because it reimplemented
    // publication itself instead of calling the real listener startup path.
    let server = Arc::new(LocalServer::open(root.path()).expect("server opens"));

    let metadata = std::fs::symlink_metadata(&socket_path).expect("published socket exists");
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    let has_staging_directory = std::fs::read_dir(root.path())
        .expect("root is readable")
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".htap-ipc-")
                && entry
                    .file_type()
                    .map(|file_type| file_type.is_dir())
                    .unwrap_or(false)
        });
    assert!(!has_staging_directory);

    drop(server);

    let error = std::fs::symlink_metadata(&socket_path).expect_err("socket is removed");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}
