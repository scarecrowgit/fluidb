use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use htap_common::{HtapError, Row, Value, Version};
use htap_rowstore::{Engine, EngineOptions, Mutation, SstOptions, Wal, WalOptions, WalRecord};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

#[test]
fn test_commit_and_snapshot_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options).unwrap();

    assert_eq!(engine.visible_version(), Version::INITIAL);
    let s0 = engine.snapshot();

    let row1 = make_row(100);
    let v2 = engine
        .commit(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: row1.clone(),
            }],
        )
        .unwrap();

    assert_eq!(v2, Version::new(2));
    assert_eq!(engine.visible_version(), Version::new(2));

    let s1 = engine.snapshot();

    // At snapshot s0, key was not yet written
    assert_eq!(engine.get(0, b"k1", s0).unwrap(), None);

    // At snapshot s1, key is visible
    assert_eq!(engine.get(0, b"k1", s1).unwrap(), Some(row1));
}

#[test]
fn test_reopen_preserves_data_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());

    let (v_last, row1, row2) = {
        let engine = Engine::open(options.clone()).unwrap();
        let s0 = engine.snapshot();

        let r1 = make_row(10);
        let r2 = make_row(20);

        let v2 = engine
            .commit(
                1,
                s0,
                vec![
                    Mutation::Put {
                        partition_id: 0,
                        key: b"key1".to_vec(),
                        row: r1.clone(),
                    },
                    Mutation::Put {
                        partition_id: 1,
                        key: b"key2".to_vec(),
                        row: r2.clone(),
                    },
                ],
            )
            .unwrap();

        assert_eq!(v2, Version::new(2));
        assert_eq!(engine.visible_version(), Version::new(2));
        (v2, r1, r2)
    };

    // Reopen from disk
    let engine2 = Engine::open(options).unwrap();
    assert_eq!(engine2.visible_version(), v_last);
    let s = engine2.snapshot();
    assert_eq!(engine2.get(0, b"key1", s).unwrap(), Some(row1));
    assert_eq!(engine2.get(1, b"key2", s).unwrap(), Some(row2));
}

#[test]
fn test_mvcc_update_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options).unwrap();

    let s0 = engine.snapshot();

    let r1 = make_row(1);
    engine
        .commit(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k".to_vec(),
                row: r1.clone(),
            }],
        )
        .unwrap();
    let s1 = engine.snapshot();

    let r2 = make_row(2);
    engine
        .commit(
            2,
            s1,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k".to_vec(),
                row: r2.clone(),
            }],
        )
        .unwrap();
    let s2 = engine.snapshot();

    let r3 = make_row(3);
    engine
        .commit(
            3,
            s2,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k".to_vec(),
                row: r3.clone(),
            }],
        )
        .unwrap();
    let s3 = engine.snapshot();

    assert_eq!(engine.get(0, b"k", s0).unwrap(), None);
    assert_eq!(engine.get(0, b"k", s1).unwrap(), Some(r1));
    assert_eq!(engine.get(0, b"k", s2).unwrap(), Some(r2));
    assert_eq!(engine.get(0, b"k", s3).unwrap(), Some(r3));
}

#[test]
fn test_delete_resurrection_regression() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options.clone()).unwrap();

    let s0 = engine.snapshot();
    let row = make_row(42);

    // Write k in SST 1
    engine
        .commit(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k".to_vec(),
                row: row.clone(),
            }],
        )
        .unwrap();
    engine.flush().unwrap();

    // Delete k in SST 2
    let s1 = engine.snapshot();
    engine
        .commit(
            2,
            s1,
            vec![Mutation::Delete {
                partition_id: 0,
                key: b"k".to_vec(),
            }],
        )
        .unwrap();
    engine.flush().unwrap();

    // At s1, row is still visible
    assert_eq!(engine.get(0, b"k", s1).unwrap(), Some(row));

    // At latest snapshot, row MUST be None and not resurrected from SST 1
    let s2 = engine.snapshot();
    assert_eq!(engine.get(0, b"k", s2).unwrap(), None);

    drop(engine);

    // Reopen: still None
    let engine2 = Engine::open(options).unwrap();
    assert_eq!(engine2.get(0, b"k", engine2.snapshot()).unwrap(), None);
}

#[test]
fn test_first_writer_wins_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options.clone()).unwrap();

    let s0 = engine.snapshot();
    let r1 = make_row(100);
    engine
        .commit(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"shared".to_vec(),
                row: r1.clone(),
            }],
        )
        .unwrap();

    let s1 = engine.snapshot();

    // Both txn2 and txn3 read from snapshot s1
    let r2 = make_row(200);
    let r3 = make_row(300);

    // txn2 commits first
    engine
        .commit(
            2,
            s1,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"shared".to_vec(),
                row: r2.clone(),
            }],
        )
        .unwrap();

    // txn3 attempts to commit using the same snapshot s1 on the same key -> conflict!
    let err = engine
        .commit(
            3,
            s1,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"shared".to_vec(),
                row: r3,
            }],
        )
        .unwrap_err();

    assert!(matches!(err, HtapError::Conflict(_)));

    // Visible value is r2
    assert_eq!(
        engine.get(0, b"shared", engine.snapshot()).unwrap(),
        Some(r2.clone())
    );

    drop(engine);

    // Reopen: only r2 is durable and visible, r3 never entered WAL
    let engine2 = Engine::open(options).unwrap();
    assert_eq!(
        engine2.get(0, b"shared", engine2.snapshot()).unwrap(),
        Some(r2)
    );
}

#[test]
fn test_snapshot_isolation_independent_keys() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options).unwrap();

    let s0 = engine.snapshot();
    engine
        .commit(
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
                    key: b"k2".to_vec(),
                    row: make_row(2),
                },
            ],
        )
        .unwrap();

    let s1 = engine.snapshot();

    // Two concurrent transactions started at s1 modify disjoint keys
    let new_r1 = make_row(10);
    let new_r2 = make_row(20);

    engine
        .commit(
            2,
            s1,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: new_r1.clone(),
            }],
        )
        .unwrap();

    engine
        .commit(
            3,
            s1,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k2".to_vec(),
                row: new_r2.clone(),
            }],
        )
        .unwrap();

    let s2 = engine.snapshot();
    assert_eq!(engine.get(0, b"k1", s2).unwrap(), Some(new_r1));
    assert_eq!(engine.get(0, b"k2", s2).unwrap(), Some(new_r2));
}

#[test]
fn test_auto_flush_and_manual_flush() {
    let dir = tempfile::tempdir().unwrap();
    // Very small memtable capacity to trigger auto-flush
    let options = EngineOptions::new(dir.path()).with_memtable_bytes(256);
    let engine = Engine::open(options.clone()).unwrap();

    // Commit enough rows to trigger auto-flush
    for i in 0..15 {
        let s = engine.snapshot();
        engine
            .commit(
                i + 1,
                s,
                vec![Mutation::Put {
                    partition_id: 0,
                    key: format!("auto_key_{i:04}").into_bytes(),
                    row: make_row(i as i64),
                }],
            )
            .unwrap();
    }

    // Check that at least one SST file exists
    let sst_dir = dir.path().join("sst");
    let sst_count = std::fs::read_dir(&sst_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "sst"))
        .count();
    assert!(sst_count >= 1, "Expected auto-flush to create SST files");

    // Manual flush remaining entries
    engine.flush().unwrap();

    drop(engine);

    // Reopen and verify all rows
    let engine2 = Engine::open(options).unwrap();
    let s = engine2.snapshot();
    for i in 0..15 {
        let expected = make_row(i as i64);
        assert_eq!(
            engine2
                .get(0, format!("auto_key_{i:04}").as_bytes(), s)
                .unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn test_flush_then_reopen_empty_wal() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path())
        .with_wal_options(WalOptions::new(dir.path().join("wal")).with_max_segment_bytes(1));
    let engine = Engine::open(options.clone()).unwrap();

    let s0 = engine.snapshot();
    let r1 = make_row(999);
    engine
        .commit(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"durable_key".to_vec(),
                row: r1.clone(),
            }],
        )
        .unwrap();

    engine.flush().unwrap();
    drop(engine);

    // Check WAL replay committed records
    let wal_replay = Wal::replay(&dir.path().join("wal")).unwrap();
    // After flush checkpoint and GC with rolled segment, superseded segments are deleted.
    // The replayed committed records are empty!
    assert_eq!(wal_replay.committed_records().len(), 0);

    // Reopen engine: data is loaded from SST, visible and intact.
    let engine2 = Engine::open(options).unwrap();
    assert_eq!(
        engine2.get(0, b"durable_key", engine2.snapshot()).unwrap(),
        Some(r1)
    );
}

#[test]
fn test_many_commits_across_several_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options.clone()).unwrap();

    for i in 0..60 {
        let s = engine.snapshot();
        engine
            .commit(
                i + 1,
                s,
                vec![Mutation::Put {
                    partition_id: i % 3,
                    key: format!("key_{}", i % 10).into_bytes(),
                    row: make_row(i as i64),
                }],
            )
            .unwrap();

        if i % 15 == 14 {
            engine.flush().unwrap();
        }
    }

    drop(engine);

    // Reopen and check the latest state
    let engine2 = Engine::open(options).unwrap();
    let s = engine2.snapshot();

    // Verify key_0 through key_9
    for k in 0..10 {
        // Find last write for this key
        let mut last_val = None;
        let mut last_part = 0;
        for i in 0..60 {
            if i % 10 == k {
                last_val = Some(i as i64);
                last_part = i % 3;
            }
        }
        let res = engine2
            .get(last_part, format!("key_{k}").as_bytes(), s)
            .unwrap();
        assert_eq!(res, last_val.map(make_row));
    }
}

#[test]
fn test_empty_batch_and_duplicate_key_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options).unwrap();

    let s0 = engine.snapshot();

    // Empty batch
    let err_empty = engine.commit(1, s0, vec![]).unwrap_err();
    assert!(matches!(err_empty, HtapError::InvalidArgument(_)));

    // Duplicate key in same batch
    let err_dup = engine
        .commit(
            2,
            s0,
            vec![
                Mutation::Put {
                    partition_id: 0,
                    key: b"k".to_vec(),
                    row: make_row(1),
                },
                Mutation::Delete {
                    partition_id: 0,
                    key: b"k".to_vec(),
                },
            ],
        )
        .unwrap_err();
    assert!(matches!(err_dup, HtapError::InvalidArgument(_)));

    // Visible version didn't change
    assert_eq!(engine.visible_version(), Version::INITIAL);
}

#[test]
fn test_orphan_tmp_and_unlisted_sst_removed() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());

    // 1. Create engine and flush one SST
    {
        let engine = Engine::open(options.clone()).unwrap();
        let s = engine.snapshot();
        engine
            .commit(
                1,
                s,
                vec![Mutation::Put {
                    partition_id: 0,
                    key: b"valid".to_vec(),
                    row: make_row(123),
                }],
            )
            .unwrap();
        engine.flush().unwrap();
    }

    // 2. Inject orphan files
    let manifest_tmp = dir.path().join("MANIFEST.tmp");
    File::create(&manifest_tmp)
        .unwrap()
        .write_all(b"garbage")
        .unwrap();

    let sst_dir = dir.path().join("sst");
    let orphan_tmp = sst_dir.join("999.sst.tmp");
    File::create(&orphan_tmp)
        .unwrap()
        .write_all(b"garbage")
        .unwrap();

    let unlisted_sst = sst_dir.join("888.sst");
    File::create(&unlisted_sst)
        .unwrap()
        .write_all(b"garbage")
        .unwrap();

    // 3. Reopen engine
    let engine2 = Engine::open(options).unwrap();

    // Verify orphans are cleaned up
    assert!(!manifest_tmp.exists(), "MANIFEST.tmp should be removed");
    assert!(!orphan_tmp.exists(), "999.sst.tmp should be removed");
    assert!(!unlisted_sst.exists(), "888.sst should be removed");

    // Legitimate data is still readable
    assert_eq!(
        engine2.get(0, b"valid", engine2.snapshot()).unwrap(),
        Some(make_row(123))
    );
}

#[test]
fn test_absent_key_across_multiple_ssts() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path());
    let engine = Engine::open(options).unwrap();

    // Create SST 1
    let s0 = engine.snapshot();
    engine
        .commit(
            1,
            s0,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"key1".to_vec(),
                row: make_row(1),
            }],
        )
        .unwrap();
    engine.flush().unwrap();

    // Create SST 2
    let s1 = engine.snapshot();
    engine
        .commit(
            2,
            s1,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"key2".to_vec(),
                row: make_row(2),
            }],
        )
        .unwrap();
    engine.flush().unwrap();

    // Create SST 3
    let s2 = engine.snapshot();
    engine
        .commit(
            3,
            s2,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"key3".to_vec(),
                row: make_row(3),
            }],
        )
        .unwrap();
    engine.flush().unwrap();

    // Query key that was never inserted
    let s3 = engine.snapshot();
    assert_eq!(engine.get(0, b"nonexistent", s3).unwrap(), None);
    assert_eq!(engine.get(1, b"key1", s3).unwrap(), None);
}

#[test]
fn test_corrupted_wal_version_mismatch_fails_open() {
    let dir = tempfile::tempdir().unwrap();
    let wal_dir = dir.path().join("wal");

    // Append a WAL record where data record version does not match commit version
    {
        let mut wal = Wal::open(WalOptions::new(&wal_dir)).unwrap();
        wal.append(&WalRecord::Put {
            txn_id: 10,
            partition_id: 0,
            key: b"corrupted".to_vec(),
            row: make_row(1),
            version: Version::new(2),
        })
        .unwrap();
        // Commit claims version 3, but data record was version 2!
        wal.append_commit(&WalRecord::Commit {
            txn_id: 10,
            version: Version::new(3),
        })
        .unwrap();
    }

    let options = EngineOptions::new(dir.path());
    let err = Engine::open(options).unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "Expected corruption error on version mismatch, got {err:?}"
    );
}

#[test]
fn test_options_builder() {
    let dir = PathBuf::from("/tmp/engine_test");
    let opts = EngineOptions::new(&dir)
        .with_memtable_bytes(1024)
        .with_sst_options(SstOptions::new().with_block_bytes(4096))
        .with_wal_options(WalOptions::new(dir.join("wal")).with_sync_on_commit(false));

    assert_eq!(opts.dir, dir);
    assert_eq!(opts.memtable_bytes, 1024);
    assert_eq!(opts.sst.block_bytes, 4096);
    assert!(!opts.wal.sync_on_commit);
}
