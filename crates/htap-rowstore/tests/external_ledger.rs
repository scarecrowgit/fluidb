use std::path::Path;
use tempfile::tempdir;

use htap_common::{HtapError, Mutation, Row, Value, Version};
use htap_rowstore::{
    Engine, EngineOptions, Manifest, ManifestLedgerEntry, ManifestSstEntry, Snapshot, WalOptions,
    FORMAT_VERSION_V1, FORMAT_VERSION_V2, MAX_APPLIED_EXTERNAL_TXNS, MAX_MANIFEST_PAYLOAD_BYTES,
    MAX_SST_COUNT,
};

fn make_row(val: i64) -> Row {
    Row::new(vec![Value::Int64(val)])
}

fn count_wal_segments(wal_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(wal_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".wal") {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

#[test]
fn test_manifest_v2_codec_invariants_and_error_handling() {
    // 1. Manifest v2 round-trip with ledger
    let mut manifest = Manifest::new();
    manifest.prepend(ManifestSstEntry {
        id: 1,
        entry_count: 10,
        min_version: Some(Version::new(2)),
        max_version: Some(Version::new(8)),
    });
    manifest
        .applied_txns
        .push(ManifestLedgerEntry::new(100, Version::new(2)));
    manifest
        .applied_txns
        .push(ManifestLedgerEntry::new(101, Version::new(5)));

    let bytes = manifest.encode().unwrap();
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        FORMAT_VERSION_V2
    );
    let decoded = Manifest::decode(&bytes).unwrap();
    assert_eq!(manifest, decoded);

    // 2. Format v1 fixture decodes as empty ledger
    let v1_bytes = manifest.encode_v1();
    assert_eq!(
        u16::from_le_bytes(v1_bytes[8..10].try_into().unwrap()),
        FORMAT_VERSION_V1
    );
    let decoded_v1 = Manifest::decode(&v1_bytes).unwrap();
    assert_eq!(decoded_v1.ssts, manifest.ssts);
    assert!(decoded_v1.applied_txns.is_empty());

    // 3. Unknown format version rejected
    let mut bad_version_bytes = bytes.clone();
    bad_version_bytes[8..10].copy_from_slice(&99u16.to_le_bytes());
    let err = Manifest::decode(&bad_version_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err
        .to_string()
        .contains("unsupported manifest format version"));

    // 4. Oversized payload rejected
    let mut oversized_payload_bytes = Vec::new();
    oversized_payload_bytes.extend_from_slice(b"HTAPMAN1");
    oversized_payload_bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
    oversized_payload_bytes.extend_from_slice(&(MAX_MANIFEST_PAYLOAD_BYTES + 1).to_le_bytes());
    oversized_payload_bytes.extend_from_slice(&0u32.to_le_bytes());
    let err = Manifest::decode(&oversized_payload_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("exceeds maximum"));

    // 5. Oversized SST count rejected
    let mut payload = Vec::new();
    payload.extend_from_slice(&(MAX_SST_COUNT + 1).to_le_bytes());
    let crc = crc32c::crc32c(&payload);
    let mut oversized_sst_bytes = Vec::new();
    oversized_sst_bytes.extend_from_slice(b"HTAPMAN1");
    oversized_sst_bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
    oversized_sst_bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    oversized_sst_bytes.extend_from_slice(&crc.to_le_bytes());
    oversized_sst_bytes.extend_from_slice(&payload);
    let err = Manifest::decode(&oversized_sst_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("SST count"));

    // 6. Oversized ledger count rejected
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_le_bytes()); // 0 SSTs
    payload.extend_from_slice(&((MAX_APPLIED_EXTERNAL_TXNS as u32) + 1).to_le_bytes());
    let crc = crc32c::crc32c(&payload);
    let mut oversized_ledger_bytes = Vec::new();
    oversized_ledger_bytes.extend_from_slice(b"HTAPMAN1");
    oversized_ledger_bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
    oversized_ledger_bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    oversized_ledger_bytes.extend_from_slice(&crc.to_le_bytes());
    oversized_ledger_bytes.extend_from_slice(&payload);
    let err = Manifest::decode(&oversized_ledger_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("ledger count"));

    // 7. Ledger count exceeds payload buffer rejected before allocation
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_le_bytes()); // 0 SSTs
    payload.extend_from_slice(&10_000u32.to_le_bytes()); // claims 10k entries
    payload.extend_from_slice(&[0u8; 16]); // only 1 entry present
    let crc = crc32c::crc32c(&payload);
    let mut short_payload_bytes = Vec::new();
    short_payload_bytes.extend_from_slice(b"HTAPMAN1");
    short_payload_bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
    short_payload_bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    short_payload_bytes.extend_from_slice(&crc.to_le_bytes());
    short_payload_bytes.extend_from_slice(&payload);
    let err = Manifest::decode(&short_payload_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err
        .to_string()
        .contains("too short for external ledger count"));

    // 8. Zero txn_id rejected
    let mut zero_txn_manifest = Manifest::new();
    zero_txn_manifest
        .applied_txns
        .push(ManifestLedgerEntry::new(0, Version::new(2)));
    let zero_bytes = zero_txn_manifest.encode().unwrap();
    let err = Manifest::decode(&zero_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("txn_id cannot be zero"));

    // 9. Duplicate txn_id rejected
    let mut dup_payload = Vec::new();
    dup_payload.extend_from_slice(&0u32.to_le_bytes()); // 0 SSTs
    dup_payload.extend_from_slice(&2u32.to_le_bytes()); // 2 entries
    dup_payload.extend_from_slice(&42u64.to_le_bytes());
    dup_payload.extend_from_slice(&2u64.to_le_bytes());
    dup_payload.extend_from_slice(&42u64.to_le_bytes()); // duplicate ID
    dup_payload.extend_from_slice(&2u64.to_le_bytes());
    let crc = crc32c::crc32c(&dup_payload);
    let mut dup_bytes = Vec::new();
    dup_bytes.extend_from_slice(b"HTAPMAN1");
    dup_bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
    dup_bytes.extend_from_slice(&(dup_payload.len() as u32).to_le_bytes());
    dup_bytes.extend_from_slice(&crc.to_le_bytes());
    dup_bytes.extend_from_slice(&dup_payload);
    let err = Manifest::decode(&dup_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("duplicate transaction id 42"));

    // 10. Trailing leftover bytes rejected
    let mut trailing_payload = dup_payload;
    trailing_payload.truncate(4 + 4 + 16); // 1 entry of 16 bytes, but update count to 1
    trailing_payload[4..8].copy_from_slice(&1u32.to_le_bytes());
    trailing_payload.extend_from_slice(b"trailing_junk");
    let crc = crc32c::crc32c(&trailing_payload);
    let mut trailing_bytes = Vec::new();
    trailing_bytes.extend_from_slice(b"HTAPMAN1");
    trailing_bytes.extend_from_slice(&FORMAT_VERSION_V2.to_le_bytes());
    trailing_bytes.extend_from_slice(&(trailing_payload.len() as u32).to_le_bytes());
    trailing_bytes.extend_from_slice(&crc.to_le_bytes());
    trailing_bytes.extend_from_slice(&trailing_payload);
    let err = Manifest::decode(&trailing_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("trailing leftover bytes"));

    // 11. CRC corruption rejected
    let mut corrupted = bytes;
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0x55;
    let err = Manifest::decode(&corrupted).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("CRC mismatch"));
}

#[test]
fn test_direct_duplicate_reapply_across_reopen() {
    let dir = tempdir().unwrap();

    // 1. Open engine, apply external txn 10 at version 2, and publish
    {
        let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
        let mutations = vec![Mutation::Put {
            partition_id: 0,
            key: b"k1".to_vec(),
            row: make_row(100),
        }];
        engine
            .apply_external(10, Version::new(2), mutations.clone())
            .unwrap();
        engine.publish(Version::new(2)).unwrap();

        assert_eq!(engine.committed_version(), Version::new(2));
        assert_eq!(engine.visible_version(), Version::new(2));

        // Reapply in same session is a no-op
        engine
            .apply_external(10, Version::new(2), mutations)
            .unwrap();
        assert_eq!(engine.committed_version(), Version::new(2));
        assert_eq!(engine.visible_version(), Version::new(2));
    }

    // 2. Reopen engine from disk
    {
        let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
        assert_eq!(engine.committed_version(), Version::new(2));
        assert_eq!(engine.visible_version(), Version::new(2));

        // Direct duplicate reapply across reopen must succeed with Ok(())
        let mutations = vec![Mutation::Put {
            partition_id: 0,
            key: b"k1".to_vec(),
            row: make_row(100),
        }];
        engine
            .apply_external(10, Version::new(2), mutations)
            .unwrap();

        // State remains completely intact and unchanged
        assert_eq!(engine.committed_version(), Version::new(2));
        assert_eq!(engine.visible_version(), Version::new(2));

        let val = engine
            .get(0, b"k1", Snapshot::new(Version::new(2)))
            .unwrap();
        assert_eq!(val, Some(make_row(100)));
    }
}

#[test]
fn test_mismatch_conflict_rejection() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();

    let mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"k1".to_vec(),
        row: make_row(100),
    }];

    // Apply txn 42 at version 2
    engine
        .apply_external(42, Version::new(2), mutations.clone())
        .unwrap();

    // Reapplying same txn_id 42 at version 3 must fail with Conflict
    let err = engine
        .apply_external(42, Version::new(3), mutations)
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict error, got {err:?}"
    );
    assert!(err
        .to_string()
        .contains("already applied at version v2, cannot reapply at v3"));

    // State remains at version 2
    assert_eq!(engine.committed_version(), Version::new(2));
}

#[test]
fn test_cap_rejection_unchanged_state() {
    let dir = tempdir().unwrap();

    // Create a manifest with MAX_APPLIED_EXTERNAL_TXNS entries
    let mut manifest = Manifest::new();
    manifest.ssts.push(ManifestSstEntry {
        id: 1,
        entry_count: 1,
        min_version: Some(Version::new(1)),
        max_version: Some(Version::new(2)),
    });
    // Fill ledger to hard cap
    for i in 1..=(MAX_APPLIED_EXTERNAL_TXNS as u64) {
        manifest
            .applied_txns
            .push(ManifestLedgerEntry::new(i, Version::new(2)));
    }

    // Write mock SST file for id 1 so Engine::open succeeds
    let sst_dir = dir.path().join("sst");
    std::fs::create_dir_all(&sst_dir).unwrap();
    let sst_path = sst_dir.join("1.sst");
    let sst_tmp = sst_dir.join("1.sst.tmp");
    let entries = vec![htap_rowstore::MemtableEntry {
        key: htap_rowstore::InternalKey {
            partition_id: 0,
            user_key: b"init".to_vec(),
            version: Version::new(2),
        },
        value: htap_rowstore::ValueKind::Put(make_row(0)),
    }];
    htap_rowstore::SstWriter::write(&sst_tmp, 1, entries, &htap_rowstore::SstOptions::default())
        .unwrap();
    std::fs::rename(&sst_tmp, &sst_path).unwrap();

    Manifest::atomic_publish(dir.path(), &manifest).unwrap();

    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
    assert_eq!(engine.committed_version(), Version::new(2));
    assert_eq!(engine.visible_version(), Version::INITIAL);

    // 1. Exact duplicate reapply of an existing transaction in the capped ledger succeeds
    let dup_mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"existing".to_vec(),
        row: make_row(1),
    }];
    engine
        .apply_external(1, Version::new(2), dup_mutations)
        .expect("exact duplicate reapply must succeed even at cap");
    assert_eq!(engine.committed_version(), Version::new(2));

    // 2. Applying a NEW nonzero external transaction when cap is reached is rejected BEFORE mutation
    let new_mutations = vec![Mutation::Put {
        partition_id: 0,
        key: b"new_key".to_vec(),
        row: make_row(999),
    }];
    let err = engine
        .apply_external(
            (MAX_APPLIED_EXTERNAL_TXNS as u64) + 100,
            Version::new(3),
            new_mutations,
        )
        .unwrap_err();

    assert!(
        matches!(err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument error on cap rejection, got {err:?}"
    );
    assert!(err.to_string().contains("cap reached"));

    // 3. Verify state remains completely untouched
    assert_eq!(engine.committed_version(), Version::new(2));
    assert_eq!(engine.visible_version(), Version::INITIAL);
    assert_eq!(
        engine
            .get(0, b"new_key", Snapshot::new(Version::new(3)))
            .unwrap(),
        None
    );
}

#[test]
fn test_tiny_wal_segment_wal_gc_target_deleted_then_exact_reapply_succeeds() {
    let dir = tempdir().unwrap();
    let wal_opts = WalOptions::new(dir.path().join("wal")).with_max_segment_bytes(512);

    let engine_opts = EngineOptions::new(dir.path()).with_wal_options(wal_opts.clone());

    let payload_large = vec![0x42u8; 300]; // Large enough to roll 512-byte segments quickly

    // 1. Open engine and apply txn 1 at v2
    let engine = Engine::open(engine_opts).unwrap();
    let mutations_txn1 = vec![Mutation::Put {
        partition_id: 0,
        key: b"txn1_key".to_vec(),
        row: Row::new(vec![Value::Bytes(payload_large.clone())]),
    }];
    engine
        .apply_external(1, Version::new(2), mutations_txn1.clone())
        .unwrap();
    engine.publish(Version::new(2)).unwrap();

    // 2. Apply txn 2 at v3 and txn 3 at v4 to force WAL rolls
    let mutations_txn2 = vec![Mutation::Put {
        partition_id: 0,
        key: b"txn2_key".to_vec(),
        row: Row::new(vec![Value::Bytes(payload_large.clone())]),
    }];
    engine
        .apply_external(2, Version::new(3), mutations_txn2)
        .unwrap();
    engine.publish(Version::new(3)).unwrap();

    let mutations_txn3 = vec![Mutation::Put {
        partition_id: 0,
        key: b"txn3_key".to_vec(),
        row: Row::new(vec![Value::Bytes(payload_large)]),
    }];
    engine
        .apply_external(3, Version::new(4), mutations_txn3)
        .unwrap();
    engine.publish(Version::new(4)).unwrap();

    // Verify WAL rolled to multiple segments before flush
    let segs_before = count_wal_segments(&wal_opts.dir);
    assert!(
        segs_before.len() > 1,
        "WAL should have rolled across multiple segments, got: {segs_before:?}"
    );
    let target_segment = &segs_before[0]; // The segment containing txn 1

    // 3. Flush the engine.
    // This writes an SST containing txn 1, 2, 3, publishes a new MANIFEST containing
    // the complete ledger (txn 1, 2, 3), checkpoints at v4, and runs WAL GC.
    engine.flush().unwrap();

    // Verify WAL GC deleted the target segment that held txn 1's commit record!
    let segs_after = count_wal_segments(&wal_opts.dir);
    assert!(
        !segs_after.contains(target_segment),
        "target segment {target_segment} should have been deleted by WAL GC. Remaining segments: {segs_after:?}"
    );

    // 4. Exact duplicate reapply of txn 1 (whose WAL segment was deleted!) must succeed as a no-op!
    engine
        .apply_external(1, Version::new(2), mutations_txn1.clone())
        .expect("exact reapply of txn 1 must succeed even after its WAL segment was deleted by GC");

    assert_eq!(engine.committed_version(), Version::new(4));
    assert_eq!(engine.visible_version(), Version::new(4));

    // 5. Reopen engine from disk and verify reapply across reopen
    drop(engine);

    let reopened = Engine::open(EngineOptions::new(dir.path()).with_wal_options(wal_opts)).unwrap();
    assert_eq!(reopened.committed_version(), Version::new(4));
    assert_eq!(reopened.visible_version(), Version::new(4));

    reopened
        .apply_external(1, Version::new(2), mutations_txn1)
        .expect("exact reapply after reopen with deleted WAL segment must succeed");

    assert_eq!(reopened.committed_version(), Version::new(4));
    assert_eq!(reopened.visible_version(), Version::new(4));

    // Verify data integrity: row exists and scan has no duplicates
    let snap = reopened.snapshot();
    let row = reopened.get(0, b"txn1_key", snap).unwrap().unwrap();
    assert_eq!(row.values().len(), 1);

    let scan = reopened.scan_partition(0, snap).unwrap();
    let txn1_entries: Vec<_> = scan
        .iter()
        .filter(|e| e.key.user_key == b"txn1_key")
        .collect();
    assert_eq!(txn1_entries.len(), 1);
}

#[test]
fn test_recovery_does_not_infer_committed_version_solely_from_ledger() {
    let dir = tempdir().unwrap();

    // Create a manifest with an external ledger entry at version 100, but no SSTs
    let mut manifest = Manifest::new();
    manifest
        .applied_txns
        .push(ManifestLedgerEntry::new(1, Version::new(100)));

    Manifest::atomic_publish(dir.path(), &manifest).unwrap();

    let engine = Engine::open(EngineOptions::new(dir.path())).unwrap();
    // committed_version must NOT be inferred solely from the ledger
    assert_eq!(engine.committed_version(), Version::INITIAL);
    assert_eq!(engine.visible_version(), Version::INITIAL);
}

#[test]
fn test_manifest_encode_cap_rejection() {
    // 1. SST count exceeding MAX_SST_COUNT is rejected
    let mut sst_manifest = Manifest::new();
    sst_manifest.ssts = vec![
        ManifestSstEntry {
            id: 1,
            entry_count: 1,
            min_version: None,
            max_version: None,
        };
        (MAX_SST_COUNT as usize) + 1
    ];
    let err = sst_manifest.encode().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("SST count"));

    // 2. Applied txns count exceeding MAX_APPLIED_EXTERNAL_TXNS is rejected
    let mut ledger_manifest = Manifest::new();
    ledger_manifest.applied_txns =
        vec![ManifestLedgerEntry::new(1, Version::new(1)); MAX_APPLIED_EXTERNAL_TXNS + 1];
    let err = ledger_manifest.encode().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("applied_txns"));

    // 3. Atomic publish propagates encode cap rejection
    let dir = tempdir().unwrap();
    let err = Manifest::atomic_publish(dir.path(), &ledger_manifest).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
}

#[test]
fn test_oversized_on_disk_manifest_rejected_before_allocation() {
    let dir = tempdir().unwrap();
    let manifest_path = dir.path().join("MANIFEST");

    // Create a 500 GiB sparse manifest file on disk
    let file = std::fs::File::create(&manifest_path).unwrap();
    file.set_len(500 * 1024 * 1024 * 1024).unwrap();
    drop(file);

    // Bounded exact reading must reject before allocation
    let err = Manifest::read_from_file(&manifest_path).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("exceeds maximum allowed"));

    // Engine::open must propagate this as corruption during recovery
    let err = Engine::open(EngineOptions::new(dir.path())).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("exceeds maximum allowed"));
}
