#![cfg(unix)]

use htap_common::HtapError;
use htap_server::{ConversionPolicy, LocalServer};

fn assert_unsupported<T>(result: std::result::Result<T, HtapError>) {
    match result {
        Err(HtapError::Unsupported(_)) => {}
        Err(error) => panic!("expected unsupported error, got: {error}"),
        Ok(_) => panic!("expected unsupported error, got success"),
    }
}

#[test]
fn ipc_client_mode_owner_only_methods_return_unsupported_without_panicking() {
    let root = tempfile::tempdir().expect("temporary root creates");
    let _owner = LocalServer::open(root.path()).expect("owner server opens");
    let mut client = LocalServer::open(root.path()).expect("client server opens");

    assert!(!client.is_owner(), "second handle must use IPC forwarding");

    // Before this fix, each of these client-mode paths panicked the whole process.
    assert_unsupported(client.compaction_tick());
    assert_unsupported(client.reclaim_tick());
    assert_unsupported(client.data_mover());
    assert_unsupported(client.txn_manager());
    assert_unsupported(client.colstore_dir());
    assert_unsupported(client.convert_table("missing_table"));
    assert_unsupported(client.convert_table_to_column("missing_table"));
    assert_unsupported(client.convert_table_to_row("missing_table"));
    assert_unsupported(client.conversion_tick(ConversionPolicy::manual()));
    assert_unsupported(client.tick());
    assert_unsupported(client.load_job("missing-job"));
    assert_unsupported(client.resume_job("missing-job"));

    let debug = format!("{client:?}");
    assert!(debug.contains("client"));

    client.set_scan_workers(0);
    assert_eq!(client.scan_workers(), htap_server::DEFAULT_SCAN_WORKERS);
}
