use std::fs;

use htap_catalog::TabletId;
use htap_colstore::SegmentOptions;
use htap_common::{ColumnDef, DataType, HtapError, Row, Schema, Value, Version};
use htap_convert::{
    manifest_path, open, resolve_segment_path, tablet_dir, validate_segment_path, write_atomic,
    write_segment, SegmentEntry, TabletColumnManifest, HEADER_LEN,
};

fn test_schema() -> Schema {
    Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".into(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap()
}

fn sample_rows(count: usize) -> Vec<Row> {
    (0..count)
        .map(|i| {
            Row::new(vec![
                Value::Int64(i as i64),
                Value::String(format!("item_{i}")),
            ])
        })
        .collect()
}

#[test]
fn test_roundtrip_empty_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let tablet_id = TabletId::new(10);
    let schema = test_schema();
    let base_version = Version::new(100);

    let manifest =
        TabletColumnManifest::new(1, tablet_id, schema.clone(), base_version, Vec::new());
    assert_eq!(manifest.segment_count(), 0);
    assert_eq!(manifest.total_rows(), 0);

    write_atomic(dir.path(), &manifest).expect("write_atomic should succeed");
    let m_path = manifest_path(dir.path(), tablet_id);
    assert!(m_path.is_file());

    let loaded = open(dir.path(), tablet_id).expect("open should succeed");
    assert_eq!(loaded, manifest);

    let manifest_ref = manifest.to_manifest_ref("tablets/tablet-10/MANIFEST");
    assert_eq!(manifest_ref.generation, 1);
    assert_eq!(manifest_ref.segment_count, 0);
    assert_eq!(manifest_ref.row_count, 0);
    assert_eq!(manifest_ref.base_version, base_version);
}

#[test]
fn test_roundtrip_with_durable_segments() {
    let dir = tempfile::tempdir().unwrap();
    let tablet_id = TabletId::new(42);
    let schema = test_schema();
    let generation = 2;
    let options = SegmentOptions::new().with_rows_per_block(10);

    let entry1 = write_segment(
        dir.path(),
        tablet_id,
        generation,
        "seg-0.col",
        &schema,
        sample_rows(25),
        &options,
    )
    .expect("write_segment 1");

    let entry2 = write_segment(
        dir.path(),
        tablet_id,
        generation,
        "seg-1.col",
        &schema,
        sample_rows(15),
        &options,
    )
    .expect("write_segment 2");

    assert_eq!(entry1.row_count, 25);
    assert_eq!(entry2.row_count, 15);
    assert!(entry1.summary.is_some());
    assert!(entry2.summary.is_some());

    let manifest = TabletColumnManifest::new(
        generation,
        tablet_id,
        schema.clone(),
        Version::new(50),
        vec![entry1.clone(), entry2.clone()],
    );
    assert_eq!(manifest.segment_count(), 2);
    assert_eq!(manifest.total_rows(), 40);

    write_atomic(dir.path(), &manifest).expect("write_atomic");
    let loaded = open(dir.path(), tablet_id).expect("open");
    assert_eq!(loaded, manifest);
    assert_eq!(loaded.total_rows(), 40);
}

#[test]
fn test_crc_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let tablet_id = TabletId::new(101);
    let schema = test_schema();

    let manifest = TabletColumnManifest::new(1, tablet_id, schema, Version::new(1), Vec::new());
    write_atomic(dir.path(), &manifest).unwrap();

    let m_path = manifest_path(dir.path(), tablet_id);
    let mut bytes = fs::read(&m_path).unwrap();

    // Corrupt a byte in the payload
    let last_idx = bytes.len() - 1;
    bytes[last_idx] ^= 0x7f;
    fs::write(&m_path, &bytes).unwrap();

    let err = open(dir.path(), tablet_id).unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected Corruption, got {err:?}"
    );
    assert!(err.to_string().contains("checksum mismatch"));
}

#[test]
fn test_truncation_and_envelope_rejection() {
    let tablet_id = TabletId::new(102);
    let schema = test_schema();

    let manifest = TabletColumnManifest::new(1, tablet_id, schema, Version::new(1), Vec::new());
    let valid_bytes = manifest.encode().unwrap();

    // 1. Header too short
    let short_bytes = &valid_bytes[..HEADER_LEN - 1];
    let err = TabletColumnManifest::decode(short_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("manifest too short"));

    // 2. Truncated payload
    let truncated_bytes = &valid_bytes[..HEADER_LEN + 5];
    let err = TabletColumnManifest::decode(truncated_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("truncated manifest file"));

    // 3. Trailing leftover bytes
    let mut extra_bytes = valid_bytes.clone();
    extra_bytes.extend_from_slice(b"extra_junk");
    let err = TabletColumnManifest::decode(&extra_bytes).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("trailing leftover bytes"));

    // 4. Invalid header magic
    let mut corrupt_magic = valid_bytes.clone();
    corrupt_magic[0] = b'X';
    let err = TabletColumnManifest::decode(&corrupt_magic).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("invalid manifest magic header"));

    // 5. Unsupported format version
    let mut corrupt_version = valid_bytes.clone();
    corrupt_version[8] = 99;
    let err = TabletColumnManifest::decode(&corrupt_version).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err
        .to_string()
        .contains("unsupported manifest format version"));
}

#[test]
fn test_path_rejection() {
    assert!(validate_segment_path("").is_err());
    assert!(validate_segment_path("   ").is_err());
    assert!(validate_segment_path("/abs/path/seg-0.col").is_err());
    assert!(validate_segment_path("\\abs\\path\\seg-0.col").is_err());
    assert!(validate_segment_path("../escape/seg-0.col").is_err());
    assert!(validate_segment_path("gen-1/../escape/seg-0.col").is_err());
    assert!(validate_segment_path("gen-1/seg-0.col").is_ok());

    let schema = test_schema();
    let bad_entry = SegmentEntry::new("../outside/seg.col", 10, None);
    let manifest = TabletColumnManifest::new(
        1,
        TabletId::new(1),
        schema,
        Version::new(1),
        vec![bad_entry],
    );
    let err = manifest.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("parent directory traversal"));
}

#[test]
fn test_duplicate_rejection() {
    let schema = test_schema();
    let entry1 = SegmentEntry::new("gen-1/seg-0.col", 10, None);
    let entry2 = SegmentEntry::new("gen-1/seg-0.col", 20, None);

    let manifest = TabletColumnManifest::new(
        1,
        TabletId::new(1),
        schema,
        Version::new(1),
        vec![entry1, entry2],
    );
    let err = manifest.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate segment path"));

    let dir = tempfile::tempdir().unwrap();
    let err = write_atomic(dir.path(), &manifest).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
}

#[test]
fn test_orphan_invisibility() {
    let dir = tempfile::tempdir().unwrap();
    let tablet_id = TabletId::new(200);
    let schema = test_schema();
    let generation = 1;
    let options = SegmentOptions::new();

    // 1. Write an authoritative segment
    let live_entry = write_segment(
        dir.path(),
        tablet_id,
        generation,
        "seg-live.col",
        &schema,
        sample_rows(10),
        &options,
    )
    .unwrap();

    let manifest = TabletColumnManifest::new(
        generation,
        tablet_id,
        schema.clone(),
        Version::new(1),
        vec![live_entry],
    );
    write_atomic(dir.path(), &manifest).unwrap();

    // 2. Create orphan files that are NOT in the manifest
    let t_dir = tablet_dir(dir.path(), tablet_id);
    let gen_dir = t_dir.join(format!("gen-{generation}"));

    // Orphan segment file
    let _orphan_entry = write_segment(
        dir.path(),
        tablet_id,
        generation,
        "seg-orphan.col",
        &schema,
        sample_rows(50),
        &options,
    )
    .unwrap();

    // Orphan temporary files
    let tmp_file = gen_dir.join("seg-unfinished.col.tmp");
    fs::write(&tmp_file, b"uncommitted temporary data").unwrap();

    let staging_file = t_dir.join("staging.tmp");
    fs::write(&staging_file, b"staging data").unwrap();

    // 3. Open manifest and verify orphan files are ignored
    let loaded = open(dir.path(), tablet_id).expect("open should succeed");
    assert_eq!(loaded.segments.len(), 1);
    assert_eq!(loaded.segments[0].path, "gen-1/seg-live.col");
    assert_eq!(loaded.total_rows(), 10);

    // Verify orphan files still exist on disk but were not adopted
    assert!(tmp_file.exists());
    assert!(staging_file.exists());
    assert!(gen_dir.join("seg-orphan.col").exists());
}

#[test]
fn test_manifest_references_only_readable_durable_segments() {
    let dir = tempfile::tempdir().unwrap();
    let tablet_id = TabletId::new(300);
    let schema = test_schema();
    let generation = 1;
    let options = SegmentOptions::new();

    // Write a valid segment
    let valid_entry = write_segment(
        dir.path(),
        tablet_id,
        generation,
        "seg-valid.col",
        &schema,
        sample_rows(10),
        &options,
    )
    .unwrap();

    // Case A: Missing segment referenced in manifest
    let ghost_entry = SegmentEntry::new("gen-1/ghost.col", 10, None);
    let manifest_missing = TabletColumnManifest::new(
        generation,
        tablet_id,
        schema.clone(),
        Version::new(1),
        vec![valid_entry.clone(), ghost_entry],
    );
    write_atomic(dir.path(), &manifest_missing).unwrap();

    let err = open(dir.path(), tablet_id).unwrap_err();
    assert!(
        matches!(err, HtapError::Io(_)),
        "expected Io error for missing segment, got {err:?}"
    );

    // Case B: Corrupted segment file referenced in manifest
    let corrupt_seg_entry = write_segment(
        dir.path(),
        tablet_id,
        generation,
        "seg-corrupt.col",
        &schema,
        sample_rows(10),
        &options,
    )
    .unwrap();
    let corrupt_path = resolve_segment_path(dir.path(), tablet_id, &corrupt_seg_entry.path);
    // Corrupt the header magic of the segment file
    let mut seg_bytes = fs::read(&corrupt_path).unwrap();
    seg_bytes[0] = b'X';
    fs::write(&corrupt_path, &seg_bytes).unwrap();

    let manifest_corrupt = TabletColumnManifest::new(
        generation,
        tablet_id,
        schema.clone(),
        Version::new(1),
        vec![corrupt_seg_entry],
    );
    write_atomic(dir.path(), &manifest_corrupt).unwrap();

    let err = open(dir.path(), tablet_id).unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected Corruption for corrupted segment, got {err:?}"
    );
    assert!(err.to_string().contains("invalid segment header magic"));

    // Case C: Row count mismatch between manifest and actual segment
    let count_mismatch_entry = SegmentEntry::new(&valid_entry.path, 999_999, None);
    let manifest_mismatch = TabletColumnManifest::new(
        generation,
        tablet_id,
        schema,
        Version::new(1),
        vec![count_mismatch_entry],
    );
    write_atomic(dir.path(), &manifest_mismatch).unwrap();

    let err = open(dir.path(), tablet_id).unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected Corruption for row count mismatch, got {err:?}"
    );
    assert!(err.to_string().contains("row count mismatch"));
}

#[test]
fn test_atomic_publication_cleans_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let tablet_id = TabletId::new(400);
    let schema = test_schema();

    let manifest = TabletColumnManifest::new(1, tablet_id, schema, Version::new(10), Vec::new());
    write_atomic(dir.path(), &manifest).unwrap();

    let t_dir = tablet_dir(dir.path(), tablet_id);
    let tmp_path = t_dir.join("MANIFEST.tmp");
    let final_path = t_dir.join("MANIFEST");

    assert!(final_path.is_file());
    assert!(
        !tmp_path.exists(),
        "MANIFEST.tmp must not remain after publish"
    );
}
