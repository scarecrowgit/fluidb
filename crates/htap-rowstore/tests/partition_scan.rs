use htap_common::{Row, Value};
use htap_rowstore::{Engine, EngineOptions, Mutation, Snapshot, ValueKind};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

#[test]
fn test_scan_partition_across_active_and_multiple_ssts() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Commit and flush to create first SST
    engine
        .commit(
            1,
            engine.snapshot(),
            vec![
                Mutation::Put {
                    partition_id: 1,
                    key: b"k1".to_vec(),
                    row: make_row(10),
                },
                Mutation::Put {
                    partition_id: 2, // different partition, should be ignored
                    key: b"k1".to_vec(),
                    row: make_row(999),
                },
            ],
        )
        .unwrap();
    engine.flush().unwrap();

    // Commit and flush to create second SST
    engine
        .commit(
            2,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 1,
                key: b"k2".to_vec(),
                row: make_row(20),
            }],
        )
        .unwrap();
    engine.flush().unwrap();

    // In active memtable (unflushed)
    engine
        .commit(
            3,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 1,
                key: b"k3".to_vec(),
                row: make_row(30),
            }],
        )
        .unwrap();

    let entries = engine.scan_partition(1, engine.snapshot()).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].key.user_key, b"k1");
    assert_eq!(entries[0].value, ValueKind::Put(make_row(10)));
    assert_eq!(entries[1].key.user_key, b"k2");
    assert_eq!(entries[1].value, ValueKind::Put(make_row(20)));
    assert_eq!(entries[2].key.user_key, b"k3");
    assert_eq!(entries[2].value, ValueKind::Put(make_row(30)));
}

#[test]
fn test_scan_partition_historical_snapshots_and_updates_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // v2: Put k1 -> 1
    let v2 = engine
        .commit(
            1,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(1),
            }],
        )
        .unwrap();

    // v3: Put k1 -> 2
    let v3 = engine
        .commit(
            2,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(2),
            }],
        )
        .unwrap();

    // v4: Delete k1
    let v4 = engine
        .commit(
            3,
            engine.snapshot(),
            vec![Mutation::Delete {
                partition_id: 0,
                key: b"k1".to_vec(),
            }],
        )
        .unwrap();

    // Scan at snapshot v2 sees only v2
    let scan_v2 = engine.scan_partition(0, Snapshot::new(v2)).unwrap();
    assert_eq!(scan_v2.len(), 1);
    assert_eq!(scan_v2[0].key.version, v2);
    assert_eq!(scan_v2[0].value, ValueKind::Put(make_row(1)));

    // Scan at snapshot v3 sees both physical MVCC versions (v3 and v2), newest first
    let scan_v3 = engine.scan_partition(0, Snapshot::new(v3)).unwrap();
    assert_eq!(scan_v3.len(), 2);
    assert_eq!(scan_v3[0].key.version, v3);
    assert_eq!(scan_v3[0].value, ValueKind::Put(make_row(2)));
    assert_eq!(scan_v3[1].key.version, v2);
    assert_eq!(scan_v3[1].value, ValueKind::Put(make_row(1)));

    // Scan at snapshot v4 sees tombstone at v4, put at v3, put at v2
    let scan_v4 = engine.scan_partition(0, Snapshot::new(v4)).unwrap();
    assert_eq!(scan_v4.len(), 3);
    assert_eq!(scan_v4[0].key.version, v4);
    assert_eq!(scan_v4[0].value, ValueKind::Delete);
    assert_eq!(scan_v4[1].key.version, v3);
    assert_eq!(scan_v4[1].value, ValueKind::Put(make_row(2)));
    assert_eq!(scan_v4[2].key.version, v2);
    assert_eq!(scan_v4[2].value, ValueKind::Put(make_row(1)));
}

#[test]
fn test_scan_partition_overlap_deduplication() {
    let dir = tempfile::tempdir().unwrap();
    let opts = EngineOptions::new(dir.path());
    let engine = Engine::open(opts.clone()).unwrap();

    let v2 = engine
        .commit(
            1,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 5,
                key: b"k".to_vec(),
                row: make_row(100),
            }],
        )
        .unwrap();

    // Flush to SST
    engine.flush().unwrap();
    drop(engine);

    // Reopening will replay from WAL into active memtable, while SST already has it
    let engine2 = Engine::open(opts).unwrap();
    let scan = engine2.scan_partition(5, Snapshot::new(v2)).unwrap();
    // Identical key/version/value across active memtable and SST must be deduplicated
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0].key.user_key, b"k");
    assert_eq!(scan[0].key.version, v2);
    assert_eq!(scan[0].value, ValueKind::Put(make_row(100)));
}
