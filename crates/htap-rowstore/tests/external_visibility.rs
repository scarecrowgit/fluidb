use htap_common::{HtapError, Row, Value, Version};
use htap_rowstore::{Engine, EngineOptions, Mutation, Snapshot};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

#[test]
fn test_prepare_rejects_empty_and_duplicate_batches_without_changing_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(engine.committed_version(), Version::INITIAL);

    // 1. Empty batch
    let s0 = engine.snapshot();
    let err_empty = engine.prepare(1, s0, vec![]).unwrap_err();
    assert!(matches!(err_empty, HtapError::InvalidArgument(_)));
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(engine.committed_version(), Version::INITIAL);

    // 2. Duplicate keys in batch
    let err_dup = engine
        .prepare(
            1,
            s0,
            vec![
                Mutation::Put {
                    partition_id: 0,
                    key: b"k1".to_vec(),
                    row: make_row(1),
                },
                Mutation::Put {
                    partition_id: 0,
                    key: b"k1".to_vec(),
                    row: make_row(2),
                },
            ],
        )
        .unwrap_err();
    assert!(matches!(err_dup, HtapError::InvalidArgument(_)));
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(engine.committed_version(), Version::INITIAL);

    // 3. Valid prepare creates prepared transaction with accessors
    let prep = engine
        .prepare(
            42,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(100),
            }],
        )
        .unwrap();
    assert_eq!(prep.txn_id(), 42);
    assert_eq!(prep.snapshot(), s0);
    assert_eq!(prep.mutations().len(), 1);

    // Engine state remains unchanged
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(engine.committed_version(), Version::INITIAL);
    assert_eq!(engine.get(0, b"k1", s0).unwrap(), None);
}

#[test]
fn test_apply_prepared_hidden_from_snapshot_get_scan_until_publish() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let s0 = engine.snapshot();
    let prep = engine
        .prepare(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(100),
            }],
        )
        .unwrap();

    let v2 = Version::new(2);
    engine.apply_prepared(prep, v2).unwrap();

    // After apply, committed is v2, but visible is still INITIAL
    assert_eq!(engine.committed_version(), v2);
    assert_eq!(engine.visible_version(), Version::INITIAL);

    // Engine snapshot is still at visible_version (INITIAL)
    assert_eq!(engine.snapshot().version, Version::INITIAL);

    // Hidden from snapshot get
    assert_eq!(engine.get(0, b"k1", engine.snapshot()).unwrap(), None);

    // Hidden EVEN with explicit Snapshot::new(v2) because get caps at visible_version
    assert_eq!(engine.get(0, b"k1", Snapshot::new(v2)).unwrap(), None);

    // Hidden from scan_partition EVEN with explicit Snapshot::new(v2)
    let scan_hidden = engine.scan_partition(0, Snapshot::new(v2)).unwrap();
    assert!(scan_hidden.is_empty());

    // Publish v2
    engine.publish(v2).unwrap();
    assert_eq!(engine.visible_version(), v2);

    // Now visible via get and scan_partition
    assert_eq!(
        engine.get(0, b"k1", Snapshot::new(v2)).unwrap(),
        Some(make_row(100))
    );
    let scan_visible = engine.scan_partition(0, Snapshot::new(v2)).unwrap();
    assert_eq!(scan_visible.len(), 1);
    assert_eq!(scan_visible[0].key.user_key, b"k1");
}

#[test]
fn test_publish_idempotent_and_failure_cases() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let s0 = engine.snapshot();
    let prep = engine
        .prepare(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(1),
            }],
        )
        .unwrap();

    let v2 = Version::new(2);
    let v3 = Version::new(3);
    let v4 = Version::new(4);

    // 1. Publishing unapplied version fails (v2 not yet applied)
    let err_unapplied = engine.publish(v2).unwrap_err();
    assert!(matches!(err_unapplied, HtapError::InvalidArgument(_)));

    // 2. Apply v2
    engine.apply_prepared(prep, v2).unwrap();

    // 3. Publishing skipped/future version (v3 or v4) fails
    let err_skipped = engine.publish(v3).unwrap_err();
    assert!(matches!(err_skipped, HtapError::InvalidArgument(_)));
    let err_future = engine.publish(v4).unwrap_err();
    assert!(matches!(err_future, HtapError::InvalidArgument(_)));

    // 4. Publish v2 succeeds
    engine.publish(v2).unwrap();
    assert_eq!(engine.visible_version(), v2);

    // 5. Repeated publish is idempotent
    assert!(engine.publish(v2).is_ok());
    assert!(engine.publish(Version::INITIAL).is_ok());
    assert_eq!(engine.visible_version(), v2);
}

#[test]
fn test_sequential_publication_and_legacy_commit_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let s0 = engine.snapshot();
    let p1 = engine
        .prepare(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(10),
            }],
        )
        .unwrap();

    let p2 = engine
        .prepare(
            2,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k2".to_vec(),
                row: make_row(20),
            }],
        )
        .unwrap();

    let v2 = Version::new(2);
    let v3 = Version::new(3);

    // Apply p1 at v2
    engine.apply_prepared(p1, v2).unwrap();

    // Legacy commit should be rejected because committed (2) != visible (1)
    let err_commit = engine
        .commit(
            3,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k3".to_vec(),
                row: make_row(30),
            }],
        )
        .unwrap_err();
    assert!(matches!(err_commit, HtapError::Conflict(_)));

    // Apply p2 at v3
    engine.apply_prepared(p2, v3).unwrap();

    // Legacy commit still rejected
    assert!(matches!(
        engine
            .commit(
                4,
                s0,
                vec![Mutation::Put {
                    partition_id: 0,
                    key: b"k4".to_vec(),
                    row: make_row(40),
                }]
            )
            .unwrap_err(),
        HtapError::Conflict(_)
    ));

    // Publish v2
    engine.publish(v2).unwrap();
    assert_eq!(engine.visible_version(), v2);

    // Legacy commit still rejected because v3 is still unpublished (committed 3 != visible 2)
    assert!(matches!(
        engine
            .commit(
                5,
                engine.snapshot(),
                vec![Mutation::Put {
                    partition_id: 0,
                    key: b"k5".to_vec(),
                    row: make_row(50),
                }]
            )
            .unwrap_err(),
        HtapError::Conflict(_)
    ));

    // Publish v3
    engine.publish(v3).unwrap();
    assert_eq!(engine.visible_version(), v3);

    // Now committed (3) == visible (3), legacy commit succeeds!
    let v4 = engine
        .commit(
            6,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k6".to_vec(),
                row: make_row(60),
            }],
        )
        .unwrap();
    assert_eq!(v4, Version::new(4));
    assert_eq!(engine.visible_version(), Version::new(4));
    assert_eq!(engine.committed_version(), Version::new(4));
}

#[test]
fn test_reopen_behavior_for_durable_applied_records() {
    let dir = tempfile::tempdir().unwrap();
    let opts = EngineOptions::new(dir.path());

    let (v2, r1) = {
        let engine = Engine::open(opts.clone()).unwrap();
        let s0 = engine.snapshot();
        let r1 = make_row(42);
        let prep = engine
            .prepare(
                1,
                s0,
                vec![Mutation::Put {
                    partition_id: 0,
                    key: b"k1".to_vec(),
                    row: r1.clone(),
                }],
            )
            .unwrap();

        let v2 = Version::new(2);
        engine.apply_prepared(prep, v2).unwrap();

        // Intentionally NOT calling publish(v2) before restart!
        assert_eq!(engine.visible_version(), Version::INITIAL);
        assert_eq!(engine.committed_version(), v2);

        (v2, r1)
    };

    // Reopen engine
    let engine2 = Engine::open(opts).unwrap();

    // Durable committed records recovered from WAL/SST become visible after restart
    assert_eq!(engine2.visible_version(), v2);
    assert_eq!(engine2.committed_version(), v2);

    let s = engine2.snapshot();
    assert_eq!(engine2.get(0, b"k1", s).unwrap(), Some(r1));
}

#[test]
fn test_concurrent_publication_serialized_cannot_skip_or_reorder() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(EngineOptions::new(dir.path())).unwrap());

    // Prepare and apply 8 transactions: v2 to v9
    let count = 8;
    for i in 1..=count {
        let v = Version::new(i + 1);
        let prep = engine
            .prepare(
                i,
                engine.snapshot(),
                vec![Mutation::Put {
                    partition_id: 0,
                    key: format!("k{i}").into_bytes(),
                    row: make_row(i as i64),
                }],
            )
            .unwrap();
        engine.apply_prepared(prep, v).unwrap();
    }

    assert_eq!(engine.committed_version(), Version::new(count + 1));
    assert_eq!(engine.visible_version(), Version::INITIAL);

    let stop_obs = Arc::new(AtomicBool::new(false));
    let engine_obs = Arc::clone(&engine);
    let stop_obs_clone = Arc::clone(&stop_obs);

    // Observer thread verifies monotonic visibility without regression or overshoot
    let observer = std::thread::spawn(move || {
        let mut last = Version::INITIAL;
        while !stop_obs_clone.load(Ordering::Acquire) {
            let curr = engine_obs.visible_version();
            assert!(
                curr >= last,
                "visible version regressed: curr={curr}, last={last}"
            );
            assert!(
                curr <= Version::new(count + 1),
                "visible version overshot committed: curr={curr}"
            );
            last = curr;
            std::hint::spin_loop();
        }
    });

    // Multiple threads race to publish versions. Each version has 2 racing publisher threads.
    let mut handles = Vec::new();
    for target in 2..=(count + 1) {
        for _ in 0..2 {
            let engine_clone = Arc::clone(&engine);
            handles.push(std::thread::spawn(move || {
                let ver = Version::new(target);
                loop {
                    match engine_clone.publish(ver) {
                        Ok(()) => {
                            // Successfully advanced watermark or already published idempotently
                            break;
                        }
                        Err(HtapError::InvalidArgument(msg)) => {
                            // Cannot skip versions
                            assert!(
                                msg.contains("expected next visible version"),
                                "unexpected error message: {msg}"
                            );
                            std::thread::yield_now();
                        }
                        Err(other) => panic!("unexpected error: {other}"),
                    }
                }
            }));
        }
    }

    for h in handles {
        h.join().expect("publisher thread panicked");
    }

    stop_obs.store(true, Ordering::Release);
    observer.join().expect("observer thread panicked");

    assert_eq!(engine.visible_version(), Version::new(count + 1));

    // Verify legacy commit also functions without deadlock under commit_lock
    let v_legacy = engine
        .commit(
            100,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k_legacy".to_vec(),
                row: make_row(999),
            }],
        )
        .unwrap();
    assert_eq!(v_legacy, Version::new(count + 2));
    assert_eq!(engine.visible_version(), Version::new(count + 2));
    assert_eq!(engine.committed_version(), Version::new(count + 2));
}
