use std::collections::HashSet;

use htap_common::{HtapError, Row, Value, Version};
use htap_rowstore::{CompactionInput, Engine, EngineOptions, Mutation, Snapshot};

fn make_row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

fn flush_put(engine: &Engine, txn_id: u64, partition_id: u64, key: &[u8], value: i64) {
    engine
        .commit(
            txn_id,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id,
                key: key.to_vec(),
                row: make_row(value),
            }],
        )
        .unwrap();
    engine.flush().unwrap();
}

fn assert_below_gc_low_water(error: HtapError, horizon: Version) {
    assert!(
        matches!(
            &error,
            HtapError::InvalidArgument(message)
                if message.contains("below GC low-water")
                    && message.contains(&horizon.to_string())
        ),
        "expected snapshot-below-GC-low-water error for horizon {horizon}, got {error}"
    );
}

#[test]
fn test_infinite_gc_horizon_is_clamped_to_committed_version() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Separate flushes ensure each version is in its own SST.
    flush_put(&engine, 1, 0, b"k", 20); // v2, SST 1
    flush_put(&engine, 2, 0, b"k", 30); // v3, SST 2

    let committed_version = engine.committed_version();
    assert_eq!(committed_version, Version::new(3));

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(HashSet::from([1, 2])),
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.collapsed_versions, 1);

    let first_version = Snapshot::new(Version::new(2));
    let error = engine.get(0, b"k", first_version).unwrap_err();
    assert_below_gc_low_water(error, committed_version);

    assert_eq!(
        engine
            .get(0, b"k", Snapshot::new(committed_version))
            .unwrap(),
        Some(make_row(30))
    );
}

#[test]
fn test_infinite_gc_horizon_does_not_exceed_visible_version() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Create an SST at v2, then leave v3 committed but unpublished.
    engine
        .apply_external(
            1,
            Version::new(2),
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k".to_vec(),
                row: make_row(20),
            }],
        )
        .unwrap();
    engine.publish(Version::new(2)).unwrap();
    engine.flush().unwrap();

    engine
        .apply_external(
            2,
            Version::new(3),
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k".to_vec(),
                row: make_row(30),
            }],
        )
        .unwrap();

    assert_eq!(engine.committed_version(), Version::new(3));
    assert_eq!(engine.visible_version(), Version::new(2));

    engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(HashSet::from([1])),
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.version, engine.visible_version());
    assert_eq!(engine.get(0, b"k", snapshot).unwrap(), Some(make_row(20)));
}
