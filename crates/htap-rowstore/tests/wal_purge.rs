use std::collections::HashSet;

use htap_common::{Row, Value, Version};
use htap_rowstore::{CompactionInput, Engine, EngineOptions, Mutation};

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

#[test]
fn test_wal_gc_does_not_resurrect_purged_partition_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
    let partition_id = 42;
    let partitions = HashSet::from([partition_id]);

    flush_put(&engine, 1, partition_id, b"a", 1);
    flush_put(&engine, 2, partition_id, b"b", 2);

    while engine
        .partitions_possibly_present_in_ssts(&partitions)
        .contains(&partition_id)
    {
        let report = engine
            .compact_once(CompactionInput {
                dropped_partition_ids: partitions.clone(),
                protected_partition_ids: HashSet::new(),
                explicit_sst_ids: None,
                gc_horizon: Version::new(u64::MAX),
            })
            .unwrap();
        assert!(
            report.compacted,
            "compaction must make progress while the dropped partition remains in SSTs"
        );
    }

    assert!(engine
        .partitions_possibly_present_in_ssts(&partitions)
        .is_empty());
    assert!(engine
        .partitions_possibly_present_in_memtables(&partitions)
        .is_empty());

    engine.flush_roll_and_gc().unwrap();
    drop(engine);

    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();

    assert!(reopened
        .partitions_possibly_present_in_ssts(&partitions)
        .is_empty());

    // This is the key WAL-purge assertion: replay must not restore rows removed
    // from the SST set by dropped-partition compaction.
    assert!(reopened
        .partitions_possibly_present_in_memtables(&partitions)
        .is_empty());
}
