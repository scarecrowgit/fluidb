use std::collections::HashSet;
use std::path::Path;

use htap_common::{HtapError, Row, Value, Version};
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

fn flush_mutations(engine: &Engine, txn_id: u64, mutations: Vec<Mutation>) {
    engine.commit(txn_id, engine.snapshot(), mutations).unwrap();
    engine.flush().unwrap();
}

fn flush_put(engine: &Engine, txn_id: u64, partition_id: u64, key: &[u8], value: i64) {
    flush_mutations(
        engine,
        txn_id,
        vec![Mutation::Put {
            partition_id,
            key: key.to_vec(),
            row: make_row(value),
        }],
    );
}

fn sst_ids(dir: &Path) -> Vec<u64> {
    manifest(dir).ssts.iter().map(|entry| entry.id).collect()
}

fn sst_contains_partition(dir: &Path, sst_id: u64, partition_id: u64) -> bool {
    let path = dir.join("sst").join(format!("{sst_id}.sst"));
    let reader = SstReader::open(path).unwrap();
    let contains = reader.iter().unwrap().any(|entry| {
        entry
            .map(|entry| entry.key.partition_id == partition_id)
            .unwrap()
    });
    contains
}

#[test]
fn test_newer_layer_wins_before_after_compaction_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let old_snapshot = engine.snapshot();
    let mut older_ids = Vec::new();
    for i in 0..4 {
        flush_mutations(
            &engine,
            i + 1,
            vec![
                Mutation::Put {
                    partition_id: 0,
                    key: b"k1".to_vec(),
                    row: make_row(i as i64),
                },
                Mutation::Put {
                    partition_id: 0,
                    key: b"k2".to_vec(),
                    row: make_row(i as i64),
                },
            ],
        );
        older_ids.push(manifest(dir.path()).ssts[0].id);
    }

    flush_mutations(
        &engine,
        10,
        vec![
            Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(100),
            },
            Mutation::Delete {
                partition_id: 0,
                key: b"k2".to_vec(),
            },
        ],
    );
    let newer_id = manifest(dir.path()).ssts[0].id;

    let snapshot = engine.snapshot();
    assert_eq!(engine.get(0, b"k1", snapshot).unwrap(), Some(make_row(100)));
    assert_eq!(engine.get(0, b"k2", snapshot).unwrap(), None);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(older_ids.iter().copied().collect()),
            gc_horizon: Version::INITIAL,
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids.len(), 4);
    assert!(!report.input_sst_ids.contains(&newer_id));

    // Under the old prepend behavior, the compacted older K1/K2 versions shadowed
    // the unselected newer Put and Delete, so these read assertions failed.
    assert_eq!(engine.get(0, b"k1", snapshot).unwrap(), Some(make_row(100)));
    assert_eq!(engine.get(0, b"k2", snapshot).unwrap(), None);

    let conflict = engine
        .commit(
            11,
            old_snapshot,
            vec![Mutation::Put {
                partition_id: 0,
                key: b"k1".to_vec(),
                row: make_row(999),
            }],
        )
        .unwrap_err();
    assert!(matches!(conflict, HtapError::Conflict(_)));

    drop(engine);

    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();
    assert_eq!(
        reopened.get(0, b"k1", reopened.snapshot()).unwrap(),
        Some(make_row(100))
    );
    assert_eq!(reopened.get(0, b"k2", reopened.snapshot()).unwrap(), None);
}

#[test]
fn test_non_newest_compaction_run_keeps_newer_ssts_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let mut older_ids = Vec::new();
    for i in 0..4 {
        flush_mutations(
            &engine,
            i + 1,
            vec![
                Mutation::Put {
                    partition_id: 0,
                    key: b"x".to_vec(),
                    row: make_row(i as i64),
                },
                Mutation::Put {
                    partition_id: 0,
                    key: b"y".to_vec(),
                    row: make_row(i as i64),
                },
                Mutation::Put {
                    partition_id: 0,
                    key: b"z".to_vec(),
                    row: make_row(i as i64),
                },
            ],
        );
        older_ids.push(manifest(dir.path()).ssts[0].id);
    }

    flush_mutations(
        &engine,
        10,
        vec![
            Mutation::Put {
                partition_id: 0,
                key: b"x".to_vec(),
                row: make_row(100),
            },
            Mutation::Put {
                partition_id: 0,
                key: b"y".to_vec(),
                row: make_row(100),
            },
        ],
    );
    let first_newer_id = manifest(dir.path()).ssts[0].id;

    flush_mutations(
        &engine,
        11,
        vec![
            Mutation::Put {
                partition_id: 0,
                key: b"y".to_vec(),
                row: make_row(200),
            },
            Mutation::Put {
                partition_id: 0,
                key: b"z".to_vec(),
                row: make_row(200),
            },
        ],
    );
    let second_newer_id = manifest(dir.path()).ssts[0].id;

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(older_ids.iter().copied().collect()),
            gc_horizon: Version::INITIAL,
        })
        .unwrap();

    assert!(report.compacted);
    let ids = sst_ids(dir.path());
    assert_eq!(ids[0], second_newer_id);
    assert_eq!(ids[1], first_newer_id);

    let snapshot = engine.snapshot();
    // Under the old prepend behavior, the output occupied index zero and its old
    // shared-key versions won these first-hit lookups.
    assert_eq!(engine.get(0, b"x", snapshot).unwrap(), Some(make_row(100)));
    assert_eq!(engine.get(0, b"y", snapshot).unwrap(), Some(make_row(200)));
    assert_eq!(engine.get(0, b"z", snapshot).unwrap(), Some(make_row(200)));
}

#[test]
fn test_compaction_output_keeps_selected_runs_original_manifest_position() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let mut selected_ids = Vec::new();
    for i in 0..4 {
        flush_put(&engine, i + 1, 0, format!("old-{i}").as_bytes(), i as i64);
        selected_ids.push(manifest(dir.path()).ssts[0].id);
    }

    flush_put(&engine, 10, 1, b"newer-a", 10);
    flush_put(&engine, 11, 1, b"newer-b", 11);

    let before = manifest(dir.path());
    let run_start = before
        .ssts
        .iter()
        .position(|entry| entry.id == *selected_ids.last().unwrap())
        .unwrap();
    assert_eq!(run_start, 2);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(selected_ids.into_iter().collect()),
            gc_horizon: Version::INITIAL,
        })
        .unwrap();

    let output_id = report.output_sst_id.unwrap();
    let after = manifest(dir.path());
    let output_index = after
        .ssts
        .iter()
        .position(|entry| entry.id == output_id)
        .unwrap();

    // Under the old prepend behavior, output_index was zero rather than run_start.
    assert_eq!(output_index, run_start);
}

#[test]
fn test_scattered_dropped_partition_is_purged_over_contiguous_passes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    for i in 0..3 {
        flush_mutations(
            &engine,
            (i * 2 + 1) as u64,
            vec![
                Mutation::Put {
                    partition_id: 7,
                    key: format!("drop-{i}").into_bytes(),
                    row: make_row(i),
                },
                Mutation::Put {
                    partition_id: 9,
                    key: b"shared".to_vec(),
                    row: make_row(i),
                },
            ],
        );
        flush_put(&engine, (i * 2 + 2) as u64, 9, b"shared", 100 + i);
    }

    let requested = HashSet::from([7]);
    assert_eq!(
        engine.partitions_possibly_present_in_ssts(&requested),
        requested
    );

    let mut passes = 0;
    while !engine
        .partitions_possibly_present_in_ssts(&requested)
        .is_empty()
    {
        assert!(passes < 8);
        let report = engine
            .compact_once(CompactionInput {
                dropped_partition_ids: requested.clone(),
                protected_partition_ids: HashSet::new(),
                explicit_sst_ids: None,
                gc_horizon: Version::new(u64::MAX),
            })
            .unwrap();
        assert!(report.compacted);
        passes += 1;
    }

    assert!(passes > 0);
    assert!(engine
        .partitions_possibly_present_in_ssts(&requested)
        .is_empty());

    for i in 0..3 {
        assert_eq!(
            engine
                .get(7, format!("drop-{i}").as_bytes(), engine.snapshot())
                .unwrap(),
            None
        );
    }

    // Rewriting the scattered SSTs must not let an older shared value shadow
    // the newest value in the live partition.
    assert_eq!(
        engine.get(9, b"shared", engine.snapshot()).unwrap(),
        Some(make_row(102))
    );
}

#[test]
fn test_sandwiched_partition_becomes_exactly_absent_in_one_pass() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_mutations(
        &engine,
        1,
        vec![
            Mutation::Put {
                partition_id: 1,
                key: b"shared".to_vec(),
                row: make_row(1),
            },
            Mutation::Put {
                partition_id: 2,
                key: b"drop".to_vec(),
                row: make_row(2),
            },
            Mutation::Put {
                partition_id: 3,
                key: b"keep".to_vec(),
                row: make_row(3),
            },
        ],
    );
    let sandwiched_id = manifest(dir.path()).ssts[0].id;

    flush_put(&engine, 2, 1, b"shared", 100);
    let newer_id = manifest(dir.path()).ssts[0].id;

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::from([2]),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids, vec![sandwiched_id]);
    let cleaned_id = report.output_sst_id.unwrap();
    assert!(!sst_contains_partition(dir.path(), cleaned_id, 2));
    assert!(engine
        .partitions_possibly_present_in_ssts(&HashSet::from([2]))
        .is_empty());

    assert_eq!(engine.get(2, b"drop", engine.snapshot()).unwrap(), None);
    assert_eq!(
        engine.get(3, b"keep", engine.snapshot()).unwrap(),
        Some(make_row(3))
    );

    // Under the old prepend behavior, the rewritten old partition-1 value moved
    // ahead of newer_id and this lookup returned 1 instead of 100.
    assert_eq!(manifest(dir.path()).ssts[0].id, newer_id);
    assert_eq!(
        engine.get(1, b"shared", engine.snapshot()).unwrap(),
        Some(make_row(100))
    );

    let before_second = manifest(dir.path());
    let second = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::from([2]),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(!second.compacted);
    assert_eq!(manifest(dir.path()), before_second);
    assert!(manifest(dir.path())
        .ssts
        .iter()
        .any(|entry| entry.id == cleaned_id));
}

#[test]
fn test_explicit_set_with_protected_middle_compacts_only_one_sub_run() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 1, b"a", 1);
    let a = manifest(dir.path()).ssts[0].id;
    flush_put(&engine, 2, 2, b"b", 2);
    let b = manifest(dir.path()).ssts[0].id;
    flush_put(&engine, 3, 3, b"c", 3);
    let c = manifest(dir.path()).ssts[0].id;

    assert_eq!(sst_ids(dir.path()), vec![c, b, a]);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::from([2]),
            explicit_sst_ids: Some(HashSet::from([a, b, c])),
            gc_horizon: Version::INITIAL,
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids, vec![c]);
    assert_ne!(report.output_sst_id, Some(c));

    let output = report.output_sst_id.unwrap();
    let ids = sst_ids(dir.path());

    // Under the old prepend/non-contiguous behavior, A and C could be rewritten
    // together across protected B instead of preserving this manifest separation.
    assert_eq!(ids, vec![output, b, a]);
    assert!(dir.path().join("sst").join(format!("{a}.sst")).exists());
    assert!(dir.path().join("sst").join(format!("{b}.sst")).exists());
    assert!(!dir.path().join("sst").join(format!("{c}.sst")).exists());
}

#[test]
fn test_selected_tombstone_never_resurrects_older_unselected_value() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 0, b"k", 10);
    let old_value_id = manifest(dir.path()).ssts[0].id;

    flush_mutations(
        &engine,
        2,
        vec![Mutation::Delete {
            partition_id: 0,
            key: b"k".to_vec(),
        }],
    );
    let tombstone_id = manifest(dir.path()).ssts[0].id;

    flush_put(&engine, 3, 1, b"newer-unrelated", 30);
    let newer_unrelated_id = manifest(dir.path()).ssts[0].id;

    assert_eq!(engine.get(0, b"k", engine.snapshot()).unwrap(), None);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(HashSet::from([tombstone_id])),
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids, vec![tombstone_id]);
    let output_id = report.output_sst_id.unwrap();

    let ids = sst_ids(dir.path());
    // Under the old prepend behavior, the rewritten tombstone output displaced
    // the unselected newer SST from manifest index zero.
    assert_eq!(ids, vec![newer_unrelated_id, output_id, old_value_id]);
    assert_eq!(engine.get(0, b"k", engine.snapshot()).unwrap(), None);

    drop(engine);

    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();
    assert_eq!(reopened.get(0, b"k", reopened.snapshot()).unwrap(), None);
}
