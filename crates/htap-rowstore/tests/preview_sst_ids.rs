use std::collections::HashSet;
use std::path::Path;

use htap_common::{Row, Value, Version};
use htap_rowstore::manifest::Manifest;
use htap_rowstore::sst::SstReader;
use htap_rowstore::{CompactionInput, Engine, EngineOptions, Mutation};

fn make_row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

fn manifest(dir: &Path) -> Manifest {
    Manifest::read_from_file(&dir.join("MANIFEST"))
        .unwrap()
        .expect("manifest must exist")
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

fn sst_ids_containing_partition(dir: &Path, partition_id: u64) -> HashSet<u64> {
    manifest(dir)
        .ssts
        .into_iter()
        .filter_map(|entry| {
            let path = dir.join("sst").join(format!("{}.sst", entry.id));
            let contains_partition = SstReader::open(path)
                .unwrap()
                .iter()
                .unwrap()
                .any(|record| record.unwrap().key.partition_id == partition_id);

            contains_partition.then_some(entry.id)
        })
        .collect()
}

#[test]
fn test_preview_sst_ids_selects_dropped_partition_ssts_for_forced_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Flush the dropped partition consecutively so its SSTs are contiguous in the
    // manifest's newest-first order. The forced pass can then return both SSTs in
    // one preview call; partition 7 remains non-dropped and follows them.
    flush_put(&engine, 1, 1000, b"first", 1);
    flush_put(&engine, 2, 1000, b"second", 2);
    flush_put(&engine, 3, 7, b"keep", 7);

    let dropped_partition_ids = HashSet::from([1000]);
    let expected_sst_ids = sst_ids_containing_partition(dir.path(), 1000);
    assert_eq!(expected_sst_ids.len(), 2);

    let preview = engine.preview_compaction_candidates(&dropped_partition_ids, &HashSet::new());

    // The old code passed dropped_partition_ids = {1000} to
    // select_compaction_candidates, whose forced pass checked
    // ssts_with_dropped_partitions.contains(&sst_id), even though
    // ssts_with_dropped_partitions was {1000} as a partition id rather than SST id.
    // Since no SST has id 1000, the forced pass returned empty sst_ids here.
    assert_eq!(
        preview.sst_ids.into_iter().collect::<HashSet<_>>(),
        expected_sst_ids
    );

    for _ in 0..3 {
        if engine
            .partitions_possibly_present_in_ssts(&dropped_partition_ids)
            .is_empty()
        {
            break;
        }

        let preview = engine.preview_compaction_candidates(&dropped_partition_ids, &HashSet::new());
        assert!(
            !preview.sst_ids.is_empty(),
            "a remaining dropped-partition SST must be previewed"
        );

        let report = engine
            .compact_once(CompactionInput {
                dropped_partition_ids: dropped_partition_ids.clone(),
                protected_partition_ids: HashSet::new(),
                explicit_sst_ids: Some(preview.sst_ids.into_iter().collect()),
                gc_horizon: Version::new(u64::MAX),
            })
            .unwrap();

        assert!(report.compacted);
    }

    assert!(engine
        .partitions_possibly_present_in_ssts(&dropped_partition_ids)
        .is_empty());
    assert_eq!(
        engine.get(7, b"keep", engine.snapshot()).unwrap(),
        Some(make_row(7))
    );
}
