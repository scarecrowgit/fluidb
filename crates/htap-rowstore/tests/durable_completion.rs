//! Tests for LSM rowstore failure hardening and durable completion.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use htap_common::{HtapError, Mutation, Row, Value, Version};
use htap_rowstore::{Engine, EngineIoOp, EngineOptions, Snapshot};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

#[test]
fn test_oneshot_sst_write_fault_durable_pending_retry_flush_publish() {
    let dir = tempfile::tempdir().unwrap();
    let tripped = Arc::new(AtomicBool::new(false));
    let tripped_clone = tripped.clone();

    let hook = Arc::new(move |op| {
        if op == EngineIoOp::SstWrite && !tripped_clone.swap(true, Ordering::SeqCst) {
            Err(HtapError::Io(std::io::Error::other(
                "injected SST write failure",
            )))
        } else {
            Ok(())
        }
    });

    let options = EngineOptions::new(dir.path())
        .with_memtable_bytes(1)
        .with_io_fault_hook(hook);
    let engine = Engine::open(options).unwrap();

    let mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"k1".to_vec(),
        row: make_row(100),
    }];

    // Commit should fail post-durability boundary at auto-flush
    let err = engine
        .commit(1, Snapshot::new(Version::INITIAL), mutations)
        .unwrap_err();
    assert!(
        err.is_durable_pending(),
        "expected DurablePending error, got {err:?}"
    );
    match &err {
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        } => {
            assert_eq!(*txn_id, 1);
            assert_eq!(*version, Version::new(2));
            assert!(
                reason.contains("automatic flush failed"),
                "unexpected reason: {reason}"
            );
        }
        _ => unreachable!(),
    }

    // Committed version is advanced, visible version is not
    assert_eq!(engine.committed_version(), Version::new(2));
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert!(engine.committed_version() > engine.visible_version());

    // Row is not visible yet
    assert_eq!(engine.get(0, b"k1", engine.snapshot()).unwrap(), None);

    // Retry flush: one-shot fault was consumed, so flush succeeds
    engine.flush().unwrap();
    assert_eq!(engine.visible_version(), Version::INITIAL);

    // Publish commit version
    engine.publish(Version::new(2)).unwrap();
    assert_eq!(engine.visible_version(), Version::new(2));

    // Now row is visible
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(100))
    );
    let scan = engine.scan_partition(0, engine.snapshot()).unwrap();
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0].key.user_key, b"k1");

    // Idempotent publish
    engine.publish(Version::new(2)).unwrap();
    assert_eq!(engine.visible_version(), Version::new(2));
    let scan2 = engine.scan_partition(0, engine.snapshot()).unwrap();
    assert_eq!(scan2.len(), 1);
}

#[test]
fn test_oneshot_visible_marker_write_fault_durable_pending_retry_publish() {
    let dir = tempfile::tempdir().unwrap();
    let tripped = Arc::new(AtomicBool::new(false));
    let tripped_clone = tripped.clone();

    let hook = Arc::new(move |op| {
        if op == EngineIoOp::VisibleMarkerWrite && !tripped_clone.swap(true, Ordering::SeqCst) {
            Err(HtapError::Io(std::io::Error::other(
                "injected visible marker write failure",
            )))
        } else {
            Ok(())
        }
    });

    let options = EngineOptions::new(dir.path()).with_io_fault_hook(hook);
    let engine = Engine::open(options).unwrap();

    let mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"k1".to_vec(),
        row: make_row(200),
    }];

    // Commit should fail at publish_locked
    let err = engine
        .commit(1, Snapshot::new(Version::INITIAL), mutations)
        .unwrap_err();
    assert!(
        err.is_durable_pending(),
        "expected DurablePending error, got {err:?}"
    );
    match &err {
        HtapError::DurablePending {
            txn_id,
            version,
            reason,
        } => {
            assert_eq!(*txn_id, 1);
            assert_eq!(*version, Version::new(2));
            assert!(
                reason.contains("publish failed"),
                "unexpected reason: {reason}"
            );
        }
        _ => unreachable!(),
    }

    // Visible watermark is NOT advanced
    assert_eq!(engine.committed_version(), Version::new(2));
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert!(engine.committed_version() > engine.visible_version());

    // Row is hidden
    assert_eq!(engine.get(0, b"k1", engine.snapshot()).unwrap(), None);

    // Later publish retry succeeds
    engine.publish(Version::new(2)).unwrap();
    assert_eq!(engine.visible_version(), Version::new(2));

    // Row is visible
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(200))
    );
    let scan = engine.scan_partition(0, engine.snapshot()).unwrap();
    assert_eq!(scan.len(), 1);

    // Idempotent publish
    engine.publish(Version::new(2)).unwrap();
    assert_eq!(engine.visible_version(), Version::new(2));
    let scan2 = engine.scan_partition(0, engine.snapshot()).unwrap();
    assert_eq!(scan2.len(), 1);
}

#[test]
fn test_drop_reopen_after_failed_flush_then_publish() {
    let dir = tempfile::tempdir().unwrap();
    let tripped = Arc::new(AtomicBool::new(false));
    let tripped_clone = tripped.clone();

    let hook = Arc::new(move |op| {
        if op == EngineIoOp::SstWrite && !tripped_clone.swap(true, Ordering::SeqCst) {
            Err(HtapError::Io(std::io::Error::other(
                "injected SST write failure",
            )))
        } else {
            Ok(())
        }
    });

    let options = EngineOptions::new(dir.path())
        .with_memtable_bytes(1)
        .with_io_fault_hook(hook);

    {
        let engine = Engine::open(options).unwrap();
        let mutations = vec![Mutation::Put {
            partition_id: 0,
            key: b"k1".to_vec(),
            row: make_row(300),
        }];
        let err = engine
            .commit(1, Snapshot::new(Version::INITIAL), mutations)
            .unwrap_err();
        assert!(err.is_durable_pending());
        // Dropping engine with un-flushed, un-published committed write
    }

    // Reopen without fault hook
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
    assert_eq!(engine.committed_version(), Version::new(2));
    assert_eq!(engine.visible_version(), Version::INITIAL);

    // Flush and then publish
    engine.flush().unwrap();
    engine.publish(Version::new(2)).unwrap();

    assert_eq!(engine.visible_version(), Version::new(2));
    assert_eq!(
        engine.get(0, b"k1", engine.snapshot()).unwrap(),
        Some(make_row(300))
    );
    let scan = engine.scan_partition(0, engine.snapshot()).unwrap();
    assert_eq!(scan.len(), 1);
}

#[test]
fn test_repeated_flush_and_recovery_no_duplicates() {
    let dir = tempfile::tempdir().unwrap();

    // 1. First cycle: commit, flush, publish
    {
        let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
        engine
            .commit(
                1,
                Snapshot::new(Version::INITIAL),
                vec![Mutation::Put {
                    partition_id: 0,
                    key: b"k1".to_vec(),
                    row: make_row(1),
                }],
            )
            .unwrap();
        engine.flush().unwrap();
        // Repeated flush when active is empty should succeed without corrupting
        engine.flush().unwrap();
    }

    // 2. Second cycle: reopen, commit again, repeated flush
    {
        let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
        assert_eq!(engine.visible_version(), Version::new(2));
        assert_eq!(
            engine.get(0, b"k1", engine.snapshot()).unwrap(),
            Some(make_row(1))
        );

        engine
            .commit(
                2,
                engine.snapshot(),
                vec![Mutation::Put {
                    partition_id: 0,
                    key: b"k2".to_vec(),
                    row: make_row(2),
                }],
            )
            .unwrap();
        engine.flush().unwrap();
        engine.flush().unwrap();

        let scan = engine.scan_partition(0, engine.snapshot()).unwrap();
        assert_eq!(scan.len(), 2);
    }

    // 3. Third cycle: reopen, verify scan has no duplicates
    {
        let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
        assert_eq!(engine.visible_version(), Version::new(3));
        let scan = engine.scan_partition(0, engine.snapshot()).unwrap();
        assert_eq!(scan.len(), 2);
        assert_eq!(scan[0].key.user_key, b"k1");
        assert_eq!(scan[1].key.user_key, b"k2");
    }
}
