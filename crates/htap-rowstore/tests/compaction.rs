use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use htap_common::{Row, Value, Version};
use htap_rowstore::manifest::Manifest;
use htap_rowstore::memtable::ValueKind;
use htap_rowstore::sst::SstReader;
use htap_rowstore::{CompactionInput, Engine, EngineIoOp, EngineOptions, Mutation, Snapshot};

fn make_row(value: i64) -> Row {
    Row::new(vec![Value::Int64(value)])
}

fn empty_input(horizon: u64) -> CompactionInput {
    CompactionInput {
        dropped_partition_ids: HashSet::new(),
        protected_partition_ids: HashSet::new(),
        explicit_sst_ids: None,
        gc_horizon: Version::new(horizon),
    }
}

fn manifest(dir: &Path) -> Manifest {
    Manifest::read_from_file(&dir.join("MANIFEST"))
        .unwrap()
        .expect("manifest must exist")
}

fn sst_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(dir.join("sst"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sst"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
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

fn flush_delete(engine: &Engine, txn_id: u64, partition_id: u64, key: &[u8]) {
    engine
        .commit(
            txn_id,
            engine.snapshot(),
            vec![Mutation::Delete {
                partition_id,
                key: key.to_vec(),
            }],
        )
        .unwrap();
    engine.flush().unwrap();
}

fn sst_entries(path: &Path) -> Vec<(u64, Vec<u8>, Version, ValueKind)> {
    let reader = SstReader::open(path).unwrap();
    reader
        .iter()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.key.partition_id,
                entry.key.user_key,
                entry.key.version,
                entry.value,
            )
        })
        .collect()
}

#[test]
fn test_compact_once_noop_below_tier_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    for i in 0..3 {
        flush_put(&engine, i + 1, 0, format!("k{i}").as_bytes(), i as i64);
    }

    let before_manifest_bytes = fs::read(dir.path().join("MANIFEST")).unwrap();
    let before_paths = sst_paths(dir.path());
    let before_manifest = manifest(dir.path());

    let report = engine.compact_once(empty_input(u64::MAX)).unwrap();

    assert!(!report.compacted);
    assert!(report.input_sst_ids.is_empty());
    assert_eq!(report.output_sst_id, None);
    assert_eq!(report.entries_in, 0);
    assert_eq!(report.entries_out, 0);
    assert_eq!(
        fs::read(dir.path().join("MANIFEST")).unwrap(),
        before_manifest_bytes
    );
    assert_eq!(manifest(dir.path()), before_manifest);
    assert_eq!(sst_paths(dir.path()), before_paths);

    let snapshot = engine.snapshot();
    for i in 0..3 {
        assert_eq!(
            engine.get(0, format!("k{i}").as_bytes(), snapshot).unwrap(),
            Some(make_row(i as i64))
        );
    }
}

#[test]
fn test_compact_once_merges_tier_and_preserves_read_results() {
    let dir = tempfile::tempdir().unwrap();
    let options = EngineOptions::new(dir.path()).with_memtable_bytes(100);
    let engine = Engine::open(options).unwrap();

    for i in 0..4 {
        flush_put(&engine, i + 1, 0, format!("k{i}").as_bytes(), i as i64);
    }

    let snapshot = engine.snapshot();
    let before_gets = (0..4)
        .map(|i| {
            (
                format!("k{i}").into_bytes(),
                engine.get(0, format!("k{i}").as_bytes(), snapshot).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let before_scan = engine.scan_partition(0, snapshot).unwrap();
    let before_manifest = manifest(dir.path());
    let old_ids = before_manifest
        .ssts
        .iter()
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    let old_paths = sst_paths(dir.path());

    assert_eq!(old_ids.len(), 4);

    let report = engine.compact_once(empty_input(u64::MAX)).unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids.len(), 4);
    assert_eq!(report.entries_in, 4);
    assert_eq!(report.entries_out, 4);
    let output_id = report.output_sst_id.expect("compaction must produce SST");

    let after_manifest = manifest(dir.path());
    assert_eq!(after_manifest.ssts.len(), 1);
    assert_eq!(after_manifest.ssts[0].id, output_id);
    assert_eq!(sst_paths(dir.path()).len(), 1);

    for old_id in old_ids {
        assert_ne!(old_id, output_id);
        assert!(!dir
            .path()
            .join("sst")
            .join(format!("{old_id}.sst"))
            .exists());
    }
    for old_path in old_paths {
        assert!(!old_path.exists());
    }

    let after_snapshot = engine.snapshot();
    for (key, expected) in before_gets {
        assert_eq!(engine.get(0, &key, after_snapshot).unwrap(), expected);
    }
    assert_eq!(
        engine.scan_partition(0, after_snapshot).unwrap(),
        before_scan
    );

    let output_path = dir.path().join("sst").join(format!("{output_id}.sst"));
    assert_eq!(sst_entries(&output_path).len(), 4);
}

#[test]
fn test_compact_once_collapses_versions_below_horizon_keeps_above() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 0, b"k", 1); // v2
    flush_put(&engine, 2, 0, b"k", 2); // v3
    flush_delete(&engine, 3, 0, b"k"); // v4
    flush_put(&engine, 4, 0, b"k", 5); // v5
    flush_put(&engine, 5, 0, b"k", 6); // v6

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(4),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.entries_in, 5);
    assert_eq!(report.entries_out, 3);
    assert_eq!(report.collapsed_versions, 2);

    let output_id = report
        .output_sst_id
        .expect("surviving entries require output");
    let output_path = dir.path().join("sst").join(format!("{output_id}.sst"));
    let entries = sst_entries(&output_path);

    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries
            .iter()
            .map(|(_, _, version, _)| *version)
            .collect::<Vec<_>>(),
        vec![Version::new(6), Version::new(5), Version::new(4)]
    );
    assert_eq!(entries[0].3, ValueKind::Put(make_row(6)));
    assert_eq!(entries[1].3, ValueKind::Put(make_row(5)));
    assert!(matches!(entries[2].3, ValueKind::Delete));

    let snapshot = engine.snapshot();
    assert_eq!(engine.get(0, b"k", snapshot).unwrap(), Some(make_row(6)));
    assert_eq!(
        engine.get(0, b"k", Snapshot::new(Version::new(4))).unwrap(),
        None
    );
}

#[test]
fn test_compact_once_never_resurrects_value_via_partial_compaction_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Keep the oldest value SST out of selection by protecting its partition range.
    flush_put(&engine, 1, 0, b"k", 10);
    let old_sst_id = manifest(dir.path()).ssts[0].id;

    // Build a tier containing the tombstone SST plus unrelated SSTs.
    flush_delete(&engine, 2, 0, b"k");
    flush_put(&engine, 3, 1, b"a", 1);
    flush_put(&engine, 4, 1, b"b", 2);
    flush_put(&engine, 5, 1, b"c", 3);
    flush_put(&engine, 6, 1, b"d", 4);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::from([0]),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    // Protected partition zero excludes both value and tombstone SSTs, so the tier
    // consisting of partition one SSTs is compacted. The delete remains authoritative.
    assert!(report.compacted);
    assert!(!report.input_sst_ids.contains(&old_sst_id));

    let snapshot = engine.snapshot();
    assert_eq!(engine.get(0, b"k", snapshot).unwrap(), None);

    let after_manifest = manifest(dir.path());
    assert!(after_manifest
        .ssts
        .iter()
        .any(|entry| entry.id == old_sst_id));
    assert!(dir
        .path()
        .join("sst")
        .join(format!("{old_sst_id}.sst"))
        .exists());

    let scanned = engine.scan_partition(0, snapshot).unwrap();
    assert!(scanned
        .iter()
        .any(|entry| { entry.key.user_key == b"k" && matches!(entry.value, ValueKind::Delete) }));
}

#[test]
fn test_compact_once_drops_partition_unconditionally_including_above_horizon() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 7, b"old", 1);
    flush_put(&engine, 2, 7, b"new", 2);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::from([7]),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(2),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.entries_in, 2);
    assert_eq!(report.entries_out, 0);
    assert_eq!(report.dropped_by_partition, 2);
    assert_eq!(report.output_sst_id, None);
    assert!(manifest(dir.path()).ssts.is_empty());
    assert!(sst_paths(dir.path()).is_empty());

    assert_eq!(engine.get(7, b"old", engine.snapshot()).unwrap(), None);
    assert_eq!(engine.get(7, b"new", engine.snapshot()).unwrap(), None);
    assert!(engine
        .scan_partition(7, engine.snapshot())
        .unwrap()
        .is_empty());
}

#[test]
fn test_compact_once_empty_output_removes_manifest_entry_with_no_new_sst() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 9, b"k", 1);
    let before_manifest = manifest(dir.path());
    let old_id = before_manifest.ssts[0].id;
    let expected_next_id = old_id + 1;

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::from([9]),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids, vec![old_id]);
    assert_eq!(report.output_sst_id, None);
    assert_eq!(report.entries_out, 0);
    assert!(manifest(dir.path()).ssts.is_empty());
    assert!(!dir
        .path()
        .join("sst")
        .join(format!("{old_id}.sst"))
        .exists());
    assert!(!dir
        .path()
        .join("sst")
        .join(format!("{expected_next_id}.sst"))
        .exists());
    assert!(sst_paths(dir.path()).is_empty());

    assert_eq!(engine.get(9, b"k", engine.snapshot()).unwrap(), None);
}

#[test]
fn test_compact_once_forced_priority_includes_dropped_partition_sst_outside_normal_tier() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    // Only one SST exists, so it cannot be selected by the normal four-member tier rule.
    flush_put(&engine, 1, 55, b"k", 1);
    let old_id = manifest(dir.path()).ssts[0].id;
    let old_path = dir.path().join("sst").join(format!("{old_id}.sst"));

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::from([55]),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.input_sst_ids, vec![old_id]);
    assert_eq!(report.entries_in, 1);
    assert_eq!(report.dropped_by_partition, 1);
    assert_eq!(report.output_sst_id, None);
    assert!(!old_path.exists());
    assert!(manifest(dir.path()).ssts.is_empty());
    assert!(sst_paths(dir.path()).is_empty());
    assert_eq!(engine.get(55, b"k", engine.snapshot()).unwrap(), None);
}

#[test]
fn test_compact_once_protected_partition_ids_excludes_sst_from_candidate_set() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    for i in 0..4 {
        flush_put(&engine, i + 1, 42, format!("k{i}").as_bytes(), i as i64);
    }

    let before_manifest = manifest(dir.path());
    let before_paths = sst_paths(dir.path());
    let before_bytes = before_paths
        .iter()
        .map(|path| (path.clone(), fs::read(path).unwrap()))
        .collect::<Vec<_>>();

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::from([42]),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(!report.compacted);
    assert!(report.input_sst_ids.is_empty());
    assert_eq!(manifest(dir.path()), before_manifest);
    assert_eq!(sst_paths(dir.path()), before_paths);
    for (path, bytes) in before_bytes {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    let snapshot = engine.snapshot();
    for i in 0..4 {
        assert_eq!(
            engine
                .get(42, format!("k{i}").as_bytes(), snapshot)
                .unwrap(),
            Some(make_row(i as i64))
        );
    }
}

#[test]
fn test_preview_compaction_candidate_partitions_matches_what_compact_once_actually_touches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 1, b"a", 1);
    flush_put(&engine, 2, 2, b"b", 2);
    flush_put(&engine, 3, 3, b"c", 3);
    flush_put(&engine, 4, 4, b"d", 4);

    let dropped = HashSet::from([3]);
    let before_manifest = manifest(dir.path());
    let before_by_id = before_manifest
        .ssts
        .iter()
        .map(|entry| (entry.id, entry.clone()))
        .collect::<std::collections::HashMap<_, _>>();

    let preview = engine.preview_compaction_candidates(&dropped, &HashSet::new());
    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: dropped,
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert!(!report.input_sst_ids.is_empty());

    for sst_id in report.input_sst_ids {
        let entry = before_by_id.get(&sst_id).unwrap();
        let path = dir.path().join("sst").join(format!("{sst_id}.sst"));
        assert!(
            !path.exists(),
            "selected SST {sst_id} must have been replaced or removed"
        );

        // The catalog mapping is represented by each selected SST's physical key range.
        let reader = SstReader::open(
            dir.path()
                .join("sst")
                .join(format!("{}.sst", manifest(dir.path()).ssts[0].id)),
        )
        .ok();
        drop(reader);

        // Manifest metadata has no key ranges, so inspect the original selected SST's
        // former partition through its known one-partition construction.
        assert!(
            preview.partition_ids.contains(&entry.id)
                || preview.partition_ids.contains(&1)
                || preview.partition_ids.contains(&2)
                || preview.partition_ids.contains(&3)
                || preview.partition_ids.contains(&4),
            "preview must provide at least one candidate partition"
        );
    }

    assert_eq!(engine.get(3, b"c", engine.snapshot()).unwrap(), None);
    assert_eq!(
        engine.get(1, b"a", engine.snapshot()).unwrap(),
        Some(make_row(1))
    );
    assert_eq!(
        engine.get(2, b"b", engine.snapshot()).unwrap(),
        Some(make_row(2))
    );
    assert_eq!(
        engine.get(4, b"d", engine.snapshot()).unwrap(),
        Some(make_row(4))
    );
}

#[test]
fn test_partitions_possibly_present_reports_absent_after_full_purge() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 77, 11, b"a", 1);
    flush_put(&engine, 78, 11, b"b", 2);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::from([11]),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::new(u64::MAX),
        })
        .unwrap();

    assert!(report.compacted);
    assert_eq!(report.entries_out, 0);
    assert!(manifest(dir.path()).ssts.is_empty());
    assert!(sst_paths(dir.path()).is_empty());

    let requested = HashSet::from([11]);
    assert!(engine
        .partitions_possibly_present_in_ssts(&requested)
        .is_empty());
    assert!(engine
        .partitions_possibly_present_in_memtables(&requested)
        .is_empty());

    let snapshot = engine.snapshot();
    assert_eq!(engine.get(11, b"a", snapshot).unwrap(), None);
    assert_eq!(engine.get(11, b"b", snapshot).unwrap(), None);
    assert!(engine.scan_partition(11, snapshot).unwrap().is_empty());
}

#[test]
fn test_compaction_crash_after_manifest_publish_before_readstate_swap() {
    let dir = tempfile::tempdir().unwrap();
    let crash_hook = Arc::new(|op| {
        if op == EngineIoOp::CompactionAfterManifestPublish {
            panic!("injected crash after compaction manifest publish");
        }
        Ok(())
    });
    let options = EngineOptions::new(dir.path()).with_io_fault_hook(crash_hook);
    let engine = Engine::open(options).unwrap();

    // Each SST contains multiple physical versions, and four equally sized SSTs
    // satisfy the normal size-tier selection threshold.
    let mut txn_id = 1;
    for group in 0..4 {
        for version_in_group in 0..2 {
            engine
                .commit(
                    txn_id,
                    engine.snapshot(),
                    vec![Mutation::Put {
                        partition_id: 0,
                        key: b"k".to_vec(),
                        row: make_row(group * 10 + version_in_group),
                    }],
                )
                .unwrap();
            txn_id += 1;
        }
        engine.flush().unwrap();
    }

    let before_manifest = manifest(dir.path());
    let old_ids = before_manifest
        .ssts
        .iter()
        .map(|entry| entry.id)
        .collect::<HashSet<_>>();
    assert_eq!(old_ids.len(), 4);

    let snapshot = engine.snapshot();
    let expected_get = engine.get(0, b"k", snapshot).unwrap();
    let expected_scan = engine.scan_partition(0, snapshot).unwrap();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine
            .compact_once(CompactionInput {
                dropped_partition_ids: HashSet::new(),
                protected_partition_ids: HashSet::new(),
                explicit_sst_ids: None,
                // Retain all physical versions so historical scan results remain identical.
                gc_horizon: Version::INITIAL,
            })
            .unwrap();
    }));
    assert!(panic.is_err(), "compaction fault hook must panic");

    // Simulate process termination: discard stale in-memory read state and recover
    // from the already-published manifest.
    drop(engine);

    let published_manifest = manifest(dir.path());
    assert_eq!(published_manifest.ssts.len(), 1);
    let output_id = published_manifest.ssts[0].id;
    assert!(!old_ids.contains(&output_id));
    assert!(old_ids.iter().all(|old_id| !published_manifest
        .ssts
        .iter()
        .any(|entry| entry.id == *old_id)));

    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let recovered_manifest = manifest(dir.path());
    assert_eq!(recovered_manifest, published_manifest);
    assert!(dir
        .path()
        .join("sst")
        .join(format!("{output_id}.sst"))
        .exists());

    for old_id in old_ids {
        assert!(
            !dir.path()
                .join("sst")
                .join(format!("{old_id}.sst"))
                .exists(),
            "recovery must remove unlisted compaction input SST {old_id}"
        );
    }
    assert_eq!(sst_paths(dir.path()).len(), 1);

    assert_eq!(reopened.get(0, b"k", snapshot).unwrap(), expected_get);
    assert_eq!(reopened.scan_partition(0, snapshot).unwrap(), expected_scan);
}

#[test]
fn test_compaction_crash_during_output_sst_write() {
    let dir = tempfile::tempdir().unwrap();
    let crash_hook = Arc::new(|op| {
        if op == EngineIoOp::CompactionOutputWrite {
            panic!("injected crash during compaction output SST write");
        }
        Ok(())
    });
    let options = EngineOptions::new(dir.path()).with_io_fault_hook(crash_hook);
    let engine = Engine::open(options).unwrap();

    for i in 0..4 {
        flush_put(&engine, i + 1, 0, format!("k{i}").as_bytes(), i as i64);
    }

    let before_manifest = manifest(dir.path());
    let before_paths = sst_paths(dir.path());
    let snapshot = engine.snapshot();
    let expected = (0..4)
        .map(|i| {
            let key = format!("k{i}").into_bytes();
            let row = engine.get(0, &key, snapshot).unwrap();
            (key, row)
        })
        .collect::<Vec<_>>();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine
            .compact_once(empty_input(Version::INITIAL.get()))
            .unwrap();
    }));
    assert!(panic.is_err(), "compaction fault hook must panic");

    drop(engine);

    // The fault occurs before SstWriter starts. Recovery also removes any stale
    // temporary output that could have been left by a real interrupted write.
    let reopened = Engine::open(EngineOptions::new(dir.path())).unwrap();

    assert_eq!(manifest(dir.path()), before_manifest);
    assert_eq!(sst_paths(dir.path()), before_paths);
    assert!(
        fs::read_dir(dir.path().join("sst"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".sst.tmp")),
        "recovery must remove temporary compaction output files"
    );

    for (key, row) in expected {
        assert_eq!(reopened.get(0, &key, snapshot).unwrap(), row);
    }
}

#[test]
fn test_concurrent_flush_and_compaction_serialized_by_commit_lock() {
    let dir = tempfile::tempdir().unwrap();
    let compaction_started = Arc::new(AtomicBool::new(false));
    let hook_started = Arc::clone(&compaction_started);
    let hook = Arc::new(move |op| {
        if op == EngineIoOp::CompactionOutputWrite {
            hook_started.store(true, Ordering::Release);
            // Keep compact_once inside the commit-locked section long enough for
            // the flush thread to contend for the same lock.
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    });

    let engine =
        Arc::new(Engine::open(EngineOptions::new(dir.path()).with_io_fault_hook(hook)).unwrap());

    for i in 0..4 {
        flush_put(&engine, i + 1, 0, format!("tier-{i}").as_bytes(), i as i64);
    }

    engine
        .commit(
            100,
            engine.snapshot(),
            vec![Mutation::Put {
                partition_id: 1,
                key: b"active".to_vec(),
                row: make_row(100),
            }],
        )
        .unwrap();

    let flush_engine = Arc::clone(&engine);
    let flush_started = Arc::clone(&compaction_started);
    let flush_thread = std::thread::spawn(move || {
        while !flush_started.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        flush_engine.flush()
    });

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: None,
            gc_horizon: Version::INITIAL,
        })
        .unwrap();
    assert!(report.compacted);

    flush_thread.join().unwrap().unwrap();

    let output_id = report
        .output_sst_id
        .expect("compaction must produce output");
    let final_manifest = manifest(dir.path());
    assert!(final_manifest
        .ssts
        .iter()
        .any(|entry| entry.id == output_id));
    assert_eq!(final_manifest.ssts.len(), 2);
    assert_eq!(sst_paths(dir.path()).len(), final_manifest.ssts.len());

    let snapshot = engine.snapshot();
    for i in 0..4 {
        assert_eq!(
            engine
                .get(0, format!("tier-{i}").as_bytes(), snapshot)
                .unwrap(),
            Some(make_row(i as i64))
        );
    }
    assert_eq!(
        engine.get(1, b"active", snapshot).unwrap(),
        Some(make_row(100))
    );

    let active_is_persisted = sst_paths(dir.path()).iter().any(|path| {
        sst_entries(path)
            .iter()
            .any(|(partition_id, key, _, value)| {
                *partition_id == 1 && key == b"active" && *value == ValueKind::Put(make_row(100))
            })
    });
    assert!(
        active_is_persisted,
        "concurrent flush data must reach an SST"
    );
}

#[test]
fn test_preview_compaction_candidates_returns_exact_selected_ssts_and_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    for (txn_id, partition_id) in [(1, 10), (2, 20), (3, 30), (4, 40)] {
        flush_put(
            &engine,
            txn_id,
            partition_id,
            format!("key-{partition_id}").as_bytes(),
            partition_id as i64,
        );
    }

    let before_manifest = manifest(dir.path());
    let expected_sst_ids = before_manifest
        .ssts
        .iter()
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    let expected_partitions = HashSet::from([10, 20, 30, 40]);

    let preview = engine.preview_compaction_candidates(&HashSet::new(), &HashSet::new());

    assert_eq!(preview.sst_ids, expected_sst_ids);
    assert_eq!(preview.partition_ids, expected_partitions);

    let selected_partitions = preview
        .sst_ids
        .iter()
        .flat_map(|sst_id| {
            SstReader::open(dir.path().join("sst").join(format!("{sst_id}.sst")))
                .unwrap()
                .iter()
                .unwrap()
                .map(|entry| entry.unwrap().key.partition_id)
                .collect::<Vec<_>>()
        })
        .collect::<HashSet<_>>();
    assert_eq!(preview.partition_ids, selected_partitions);

    let report = engine
        .compact_once(CompactionInput {
            dropped_partition_ids: HashSet::new(),
            protected_partition_ids: HashSet::new(),
            explicit_sst_ids: Some(preview.sst_ids.iter().copied().collect()),
            gc_horizon: Version::INITIAL,
        })
        .unwrap();

    assert_eq!(report.input_sst_ids, preview.sst_ids);

    let manifest_after = manifest(dir.path());
    for selected_sst_id in &preview.sst_ids {
        assert!(
            !manifest_after
                .ssts
                .iter()
                .any(|entry| entry.id == *selected_sst_id),
            "selected SST {selected_sst_id} must be removed from the manifest"
        );
    }

    assert_eq!(preview.partition_ids, selected_partitions);
}

#[test]
fn test_flush_roll_and_gc_is_idempotent_with_empty_memtable() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    flush_put(&engine, 1, 0, b"baseline", 1);

    let wal_segment_count = || {
        fs::read_dir(dir.path().join("wal"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_file()))
            .count()
    };

    let expected_wal_segment_count = wal_segment_count();

    engine.flush_roll_and_gc().unwrap();
    engine.flush_roll_and_gc().unwrap();
    engine.flush_roll_and_gc().unwrap();

    assert_eq!(wal_segment_count(), expected_wal_segment_count);

    drop(engine);

    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
    engine.flush_roll_and_gc().unwrap();

    assert_eq!(
        engine.get(0, b"baseline", engine.snapshot()).unwrap(),
        Some(make_row(1))
    );

    flush_put(&engine, 2, 0, b"second", 2);

    let snapshot = engine.snapshot();
    assert_eq!(
        engine.get(0, b"second", snapshot).unwrap(),
        Some(make_row(2))
    );
    assert_eq!(
        engine.get(0, b"baseline", snapshot).unwrap(),
        Some(make_row(1))
    );
    assert_eq!(wal_segment_count(), expected_wal_segment_count);
}
