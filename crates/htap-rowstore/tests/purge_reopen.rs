use std::collections::HashSet;

use htap_common::{Row, Value, Version};
use htap_rowstore::{CompactionInput, Engine, EngineOptions, Mutation, Wal, WalRecord};

fn make_row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

#[test]
fn test_dropped_newest_partition_reopens_with_manifest_v3_high_water() {
    const OTHER_PARTITION: u64 = 7;
    const DROPPED_PARTITION: u64 = 42;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let older_version = engine
        .commit(
            1,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: OTHER_PARTITION,
                key: b"older".to_vec(),
                row: make_row(1),
            }],
        )
        .unwrap();

    let version_v = engine
        .commit(
            2,
            engine.snapshot(),
            vec![
                Mutation::Put {
                    partition_id: DROPPED_PARTITION,
                    key: b"row-1".to_vec(),
                    row: make_row(10),
                },
                Mutation::Put {
                    partition_id: DROPPED_PARTITION,
                    key: b"row-2".to_vec(),
                    row: make_row(20),
                },
            ],
        )
        .unwrap();

    assert!(version_v > older_version);
    assert_eq!(engine.committed_version(), version_v);

    engine.flush_roll_and_gc().unwrap();

    let replay = Wal::replay(&dir.path().join("wal")).unwrap();
    assert!(
        !replay.records.iter().any(|(_, record)| {
            matches!(
                record,
                WalRecord::Commit { version, .. } if *version == version_v
            )
        }),
        "WAL GC must remove the commit record for the newest version"
    );

    let dropped = HashSet::from([DROPPED_PARTITION]);
    for _ in 0..16 {
        if engine
            .partitions_possibly_present_in_ssts(&dropped)
            .is_empty()
        {
            break;
        }

        let report = engine
            .compact_once(CompactionInput {
                dropped_partition_ids: dropped.clone(),
                protected_partition_ids: HashSet::new(),
                explicit_sst_ids: None,
                gc_horizon: Version::new(u64::MAX),
            })
            .unwrap();
        assert!(
            report.compacted,
            "compaction must make progress while the dropped partition remains"
        );
    }

    assert!(
        engine
            .partitions_possibly_present_in_ssts(&dropped)
            .is_empty(),
        "dropped partition must be exactly absent from all SSTs"
    );
    assert!(
        engine
            .partitions_possibly_present_in_memtables(&dropped)
            .is_empty(),
        "dropped partition must be exactly absent from all memtables"
    );

    drop(engine);

    // Before manifest v3 persisted committed_version_high_water, recovery derived
    // the committed version from surviving SSTs and the WAL. Purging the only
    // entries at version_v while also GCing their WAL record made that derived
    // version older than VISIBLE, so open rejected the store as corrupt.
    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();

    assert!(reopened.committed_version() >= version_v);

    let next_version = reopened
        .commit(
            3,
            reopened.snapshot(),
            vec![Mutation::Put {
                partition_id: OTHER_PARTITION,
                key: b"after-reopen".to_vec(),
                row: make_row(2),
            }],
        )
        .unwrap();

    assert!(next_version > version_v);
}
