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
fn test_gc_low_water_rejects_old_snapshots_and_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Commits start at v2. Four equally sized SSTs trigger size-tiered compaction.
    flush_put(&engine, 1, 0, b"k", 20); // v2
    flush_put(&engine, 2, 0, b"k", 30); // v3
    flush_put(&engine, 3, 0, b"k", 40); // v4
    flush_put(&engine, 4, 0, b"k", 50); // v5

    let horizon = Version::new(4);
    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: horizon,
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.entries_in, 4);
    assert_eq!(report.entries_out, 2);
    assert_eq!(report.collapsed_versions, 2);

    let below_horizon = Snapshot::new(Version::new(3));

    let get_error = engine.get(0, b"k", below_horizon).unwrap_err();
    assert_below_gc_low_water(get_error, horizon);

    let scan_error = engine.scan_partition(0, below_horizon).unwrap_err();
    assert_below_gc_low_water(scan_error, horizon);

    assert_eq!(
        engine.get(0, b"k", Snapshot::new(horizon)).unwrap(),
        Some(make_row(40))
    );
    assert_eq!(
        engine.get(0, b"k", Snapshot::new(Version::new(5))).unwrap(),
        Some(make_row(50))
    );

    let at_horizon = engine.scan_partition(0, Snapshot::new(horizon)).unwrap();
    assert_eq!(at_horizon.len(), 1);
    assert_eq!(at_horizon[0].key.version, horizon);
    assert_eq!(
        at_horizon[0].value,
        htap_rowstore::ValueKind::Put(make_row(40))
    );

    let above_horizon = engine
        .scan_partition(0, Snapshot::new(Version::new(5)))
        .unwrap();
    assert_eq!(above_horizon.len(), 2);
    assert_eq!(above_horizon[0].key.version, Version::new(5));
    assert_eq!(
        above_horizon[0].value,
        htap_rowstore::ValueKind::Put(make_row(50))
    );
    assert_eq!(above_horizon[1].key.version, horizon);
    assert_eq!(
        above_horizon[1].value,
        htap_rowstore::ValueKind::Put(make_row(40))
    );

    drop(engine);

    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // This assertion would fail without the R3 fix because reopening would lose
    // the GC low-water and permit a stale read against compacted-away history.
    let reopened_error = reopened.get(0, b"k", below_horizon).unwrap_err();
    assert_below_gc_low_water(reopened_error, horizon);

    assert_eq!(
        reopened.get(0, b"k", Snapshot::new(horizon)).unwrap(),
        Some(make_row(40))
    );
    assert_eq!(
        reopened
            .get(0, b"k", Snapshot::new(Version::new(5)))
            .unwrap(),
        Some(make_row(50))
    );
}
