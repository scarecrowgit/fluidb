//! Tests for catalog validation, serialization, crash safety, and recovery.

use std::fs;

use htap_catalog::local::{encode_snapshot, FORMAT_VERSION, HEADER_MAGIC};
use htap_catalog::*;
use htap_common::{ColumnDef, DataType, HtapError, Schema, Value, Version};
use tempfile::TempDir;

fn make_schema() -> Schema {
    Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".to_string(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap()
}

fn make_valid_snapshot(generation: u64) -> CatalogSnapshot {
    let schema = make_schema();
    let table = TableDescriptor::new(
        TableId::new(1),
        "users",
        schema,
        vec![0],
        vec![PartitionId::new(10)],
        generation,
    );
    let partition = PartitionDescriptor::new(
        PartitionId::new(10),
        TableId::new(1),
        "p0",
        StorageDescriptor::Row,
        vec![TabletId::new(100)],
        generation,
    );
    let tablet = TabletDescriptor::new(
        TabletId::new(100),
        PartitionId::new(10),
        0,
        vec![ReplicaId::new(1000)],
        generation,
    );
    let replica = ReplicaDescriptor::new(
        ReplicaId::new(1000),
        TabletId::new(100),
        NodeId::new(42),
        true,
        true,
        generation,
    );

    CatalogSnapshot::new(
        generation,
        vec![table],
        vec![partition],
        vec![tablet],
        vec![replica],
    )
}

#[test]
fn test_empty_load() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let snap = store.load().unwrap();
    assert!(snap.is_none());
    assert_eq!(store.current_generation().unwrap(), 0);
    assert!(store.list_tables().unwrap().is_empty());
    assert!(store.get_table(TableId::new(1)).unwrap().is_none());
    assert!(store.get_table_by_name("users").unwrap().is_none());
}

#[test]
fn test_create_and_reopen() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let snap1 = make_valid_snapshot(1);
    store.compare_and_set(0, snap1.clone()).unwrap();

    assert_eq!(store.current_generation().unwrap(), 1);
    let loaded = store.load().unwrap().expect("snapshot should exist");
    assert_eq!(loaded, snap1);

    let tbl = store.get_table(TableId::new(1)).unwrap().unwrap();
    assert_eq!(tbl.name, "users");
    assert_eq!(tbl.primary_key, vec![0]);

    let tbl_by_name = store.get_table_by_name("users").unwrap().unwrap();
    assert_eq!(tbl_by_name.id, TableId::new(1));

    let tables = store.list_tables().unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].id, TableId::new(1));

    // Reopen store from disk
    drop(store);
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered = reopened
        .load()
        .unwrap()
        .expect("snapshot should be recovered");
    assert_eq!(recovered, snap1);
    assert_eq!(reopened.current_generation().unwrap(), 1);
}

#[test]
fn test_stale_generation_rejection() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let snap1 = make_valid_snapshot(1);

    // Mismatch when empty: expected 1, current is 0
    let err = store.compare_and_set(1, snap1.clone()).unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));

    // Next generation not strictly greater than expected
    let snap0 = make_valid_snapshot(0);
    let err = store.compare_and_set(0, snap0).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // Valid CAS 0 -> 1
    store.compare_and_set(0, snap1).unwrap();

    // Stale generation expected = 0, current = 1
    let snap2 = make_valid_snapshot(2);
    let err = store.compare_and_set(0, snap2.clone()).unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));

    // Next generation equal to expected: 1 -> 1
    let snap1_dup = make_valid_snapshot(1);
    let err = store.compare_and_set(1, snap1_dup).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // Next generation less than expected: 1 -> 0
    let snap0 = make_valid_snapshot(0);
    let err = store.compare_and_set(1, snap0).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // Valid advance 1 -> 5
    let snap5 = make_valid_snapshot(5);
    store.compare_and_set(1, snap5).unwrap();
    assert_eq!(store.current_generation().unwrap(), 5);

    // Stale expected = 1
    let snap6 = make_valid_snapshot(6);
    let err = store.compare_and_set(1, snap6).unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));

    // Disk remains at generation 5
    assert_eq!(store.current_generation().unwrap(), 5);
}

#[test]
fn test_duplicate_ids_and_names() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // 1. Duplicate TableId
    let mut snap = make_valid_snapshot(1);
    let mut t2 = snap.tables[0].clone();
    t2.name = "users2".to_string();
    snap.tables.push(t2);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate table id"));

    // 2. Duplicate Table name
    let mut snap = make_valid_snapshot(1);
    let mut t2 = snap.tables[0].clone();
    t2.id = TableId::new(2);
    snap.tables.push(t2);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate table name"));

    // 3. Empty Table name
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].name = "  ".to_string();
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("empty name"));

    // 4. Duplicate PartitionId
    let mut snap = make_valid_snapshot(1);
    let p2 = PartitionDescriptor::new(
        PartitionId::new(10), // duplicate
        TableId::new(1),
        "p1",
        StorageDescriptor::Row,
        vec![],
        1,
    );
    snap.partitions.push(p2);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate partition id"));

    // 5. Duplicate Partition name in the same table
    let mut snap = make_valid_snapshot(1);
    let p2 = PartitionDescriptor::new(
        PartitionId::new(11),
        TableId::new(1),
        "p0", // duplicate name in table 1
        StorageDescriptor::Row,
        vec![],
        1,
    );
    snap.partitions.push(p2);
    snap.tables[0].partitions.push(PartitionId::new(11));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate partition name"));

    // 6. Empty Partition name
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].name = "".to_string();
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("empty name"));

    // 7. Duplicate TabletId
    let mut snap = make_valid_snapshot(1);
    let t2 = TabletDescriptor::new(TabletId::new(100), PartitionId::new(10), 1, vec![], 1);
    snap.tablets.push(t2);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate tablet id"));

    // 8. Duplicate ReplicaId
    let mut snap = make_valid_snapshot(1);
    let r2 = ReplicaDescriptor::new(
        ReplicaId::new(1000),
        TabletId::new(100),
        NodeId::new(43),
        false,
        true,
        1,
    );
    snap.replicas.push(r2);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate replica id"));

    // Store was never modified by failed attempts
    assert!(store.load().unwrap().is_none());
}

#[test]
fn test_invalid_pk_and_schema() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // 1. Primary key index out of bounds
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].primary_key = vec![5]; // schema has 2 columns (0, 1)
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("out of bounds"));

    // 2. Duplicate primary key index
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].primary_key = vec![0, 0];
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate primary key index"));

    // 3. Empty primary key
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].primary_key = vec![];
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("empty primary key"));

    // 4. PK index referencing column not marked as primary key
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].primary_key = vec![1]; // col 1 is 'val' with primary_key: false
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("not marked as primary key"));

    // 5. Schema column marked primary key but missing from table primary_key list
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".to_string(),
            data_type: DataType::String,
            nullable: false,
            primary_key: true,
        },
    ])
    .unwrap();
    snap.tables[0].primary_key = vec![0]; // only lists index 0, missing index 1
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("absent from primary key list"));

    // 6. Schema with empty column name
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].schema = Schema::new(vec![ColumnDef {
        name: "  ".to_string(),
        data_type: DataType::Int32,
        nullable: false,
        primary_key: false,
    }])
    .unwrap();
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("empty name"));

    // 7. Converting storage descriptor roundtrip
    let mut snap_converting = make_valid_snapshot(1);
    snap_converting.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap_converting.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SnapshotPinned,
    ));
    store.compare_and_set(0, snap_converting.clone()).unwrap();
    let loaded = store.load().unwrap().unwrap();
    assert_eq!(loaded, snap_converting);
}

#[test]
fn test_foreign_key_and_ownership_validation() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // 1. Partition references nonexistent table
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].table_id = TableId::new(999);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("nonexistent table"));

    // 2. Table lists nonexistent partition
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitions.push(PartitionId::new(999));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("nonexistent partition"));

    // 3. Table lists duplicate partition
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitions.push(PartitionId::new(10));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate partition id"));

    // 4. Partition belongs to table 1, but table 2 claims it
    let mut snap = make_valid_snapshot(1);
    let t2 = TableDescriptor::new(
        TableId::new(2),
        "orders",
        make_schema(),
        vec![0],
        vec![PartitionId::new(10)], // claims table 1's partition
        1,
    );
    snap.tables.push(t2);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("belonging to table id 1"));

    // 5. Orphan partition: partition exists with table_id 1, but table 1 doesn't list it
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitions.clear();
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("not referenced by parent table"));

    // 6. Tablet references nonexistent partition
    let mut snap = make_valid_snapshot(1);
    snap.tablets[0].partition_id = PartitionId::new(999);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("nonexistent partition"));

    // 7. Partition lists nonexistent tablet
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].tablets.push(TabletId::new(999));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("nonexistent tablet"));

    // 8. Orphan tablet: tablet exists with partition_id 10, but partition doesn't list it
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].tablets.clear();
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("not referenced by parent partition"));

    // 9. Replica references nonexistent tablet
    let mut snap = make_valid_snapshot(1);
    snap.replicas[0].tablet_id = TabletId::new(999);
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("nonexistent tablet"));

    // 10. Tablet lists nonexistent replica
    let mut snap = make_valid_snapshot(1);
    snap.tablets[0].replicas.push(ReplicaId::new(999));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("nonexistent replica"));

    // 11. Orphan replica: replica exists with tablet_id 100, but tablet doesn't list it
    let mut snap = make_valid_snapshot(1);
    snap.tablets[0].replicas.clear();
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("not referenced by parent tablet"));
}

#[test]
fn test_corruption_and_truncation() {
    let temp = TempDir::new().unwrap();
    let catalog_path = temp.path().join("CATALOG");

    // 1. Truncated to 0 bytes
    fs::write(&catalog_path, b"").unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 2. Truncated header (< 18 bytes)
    fs::write(&catalog_path, b"HTAPCAT").unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 3. Bad magic bytes
    let snap = make_valid_snapshot(1);
    let mut encoded = encode_snapshot(&snap).unwrap();
    encoded[0..8].copy_from_slice(b"BADMAGIC");
    fs::write(&catalog_path, &encoded).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("invalid catalog header magic"));

    // 4. Unsupported version
    let mut encoded = encode_snapshot(&snap).unwrap();
    encoded[8..10].copy_from_slice(&2u16.to_le_bytes());
    fs::write(&catalog_path, &encoded).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err
        .to_string()
        .contains("unsupported catalog format version"));

    // 5. Huge payload length exceeding maximum allowed
    let mut encoded = encode_snapshot(&snap).unwrap();
    encoded[10..14].copy_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
    fs::write(&catalog_path, &encoded).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("exceeds maximum limit"));

    // 6. Truncated payload bytes
    let encoded = encode_snapshot(&snap).unwrap();
    fs::write(&catalog_path, &encoded[..encoded.len() - 10]).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("truncated catalog file"));

    // 7. Bit flip in payload (checksum mismatch)
    let mut encoded = encode_snapshot(&snap).unwrap();
    let last_byte_idx = encoded.len() - 1;
    encoded[last_byte_idx] ^= 0x01;
    fs::write(&catalog_path, &encoded).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("checksum mismatch"));

    // 8. Trailing leftover garbage bytes
    let mut encoded = encode_snapshot(&snap).unwrap();
    encoded.extend_from_slice(b"extra garbage");
    fs::write(&catalog_path, &encoded).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("trailing leftover bytes"));

    // 9. Malformed JSON payload with valid CRC
    let bad_json = b"{\"not\": \"a valid snapshot\"}";
    let bad_crc = crc32c::crc32c(bad_json);
    let mut raw = Vec::new();
    raw.extend_from_slice(HEADER_MAGIC);
    raw.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    raw.extend_from_slice(&(bad_json.len() as u32).to_le_bytes());
    raw.extend_from_slice(&bad_crc.to_le_bytes());
    raw.extend_from_slice(bad_json);
    fs::write(&catalog_path, &raw).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));

    // 10. Valid JSON structure that violates catalog invariants (e.g. orphan replica)
    let mut orphan_snap = make_valid_snapshot(1);
    orphan_snap.replicas.push(ReplicaDescriptor::new(
        ReplicaId::new(9999),
        TabletId::new(100),
        NodeId::new(1),
        false,
        true,
        1,
    ));
    let orphan_json = serde_json::to_vec(&orphan_snap).unwrap();
    let orphan_crc = crc32c::crc32c(&orphan_json);
    let mut raw = Vec::new();
    raw.extend_from_slice(HEADER_MAGIC);
    raw.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    raw.extend_from_slice(&(orphan_json.len() as u32).to_le_bytes());
    raw.extend_from_slice(&orphan_crc.to_le_bytes());
    raw.extend_from_slice(&orphan_json);
    fs::write(&catalog_path, &raw).unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("integrity validation"));
}

#[test]
fn test_catalog_file_bounds_oversized_and_short() {
    use htap_catalog::local::{HEADER_LEN, MAX_CATALOG_PAYLOAD_BYTES};

    let temp = TempDir::new().unwrap();
    let catalog_path = temp.path().join("CATALOG");

    // Oversized physical file rejected before allocation
    let file = fs::File::create(&catalog_path).unwrap();
    let oversized_len = (HEADER_LEN as u64) + (MAX_CATALOG_PAYLOAD_BYTES as u64) + 1;
    file.set_len(oversized_len).unwrap();
    drop(file);

    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
    assert!(err.to_string().contains("exceeds maximum allowed bound"));

    // Short header file (< HEADER_LEN) rejected as corruption
    fs::write(&catalog_path, b"SHORT").unwrap();
    let err = LocalCatalogStore::open(temp.path()).unwrap_err();
    assert!(matches!(err, HtapError::Corruption(_)));
}

#[test]
fn test_atomic_replacement_and_failed_cas_no_disk_change() {
    let temp = TempDir::new().unwrap();
    let catalog_path = temp.path().join("CATALOG");
    let tmp_path = temp.path().join("CATALOG.tmp");

    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // Publish initial snapshot gen 1
    let snap1 = make_valid_snapshot(1);
    store.compare_and_set(0, snap1.clone()).unwrap();
    assert!(catalog_path.exists());
    assert!(!tmp_path.exists());

    let catalog_bytes_before = fs::read(&catalog_path).unwrap();

    // 1. Stale generation CAS failure
    let snap_stale = make_valid_snapshot(2);
    let err = store.compare_and_set(0, snap_stale).unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));
    assert!(!tmp_path.exists());
    assert_eq!(fs::read(&catalog_path).unwrap(), catalog_bytes_before);

    // 2. Semantic validation failure (invalid PK out of bounds)
    let mut invalid_snap = make_valid_snapshot(2);
    invalid_snap.tables[0].primary_key = vec![99];
    let err = store.compare_and_set(1, invalid_snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(!tmp_path.exists());
    assert_eq!(fs::read(&catalog_path).unwrap(), catalog_bytes_before);

    // 3. Successful CAS to gen 2
    let snap2 = make_valid_snapshot(2);
    store.compare_and_set(1, snap2.clone()).unwrap();
    assert!(!tmp_path.exists());
    let catalog_bytes_after = fs::read(&catalog_path).unwrap();
    assert_ne!(catalog_bytes_before, catalog_bytes_after);

    // Reopen and verify recovered snapshot is gen 2
    drop(store);
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    assert_eq!(reopened.current_generation().unwrap(), 2);
    assert_eq!(reopened.load().unwrap().unwrap(), snap2);
}

#[test]
fn test_conversion_metadata_and_manifest_roundtrip_and_reopen() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // Initial Row-format snapshot at generation 1
    let snap1 = make_valid_snapshot(1);
    store.compare_and_set(0, snap1.clone()).unwrap();

    // Step 1: Advance to Converting state at generation 2
    let mut snap2 = snap1.clone();
    snap2.generation = 2;
    snap2.partitions[0].generation = 2;
    snap2.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 2,
    };
    snap2.partitions[0].conversion = Some(ConversionDescriptor::new(
        2,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(42),
        ConversionPhase::SegmentsWritten,
    ));
    snap2.tablets[0].generation = 2;
    snap2.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        2,
        "manifests/part_10_tablet_100.json",
        Version::new(42),
        5,
        250_000,
    ));

    store.compare_and_set(1, snap2.clone()).unwrap();

    // Reopen and verify Converting snapshot recovered cleanly from disk
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered2 = reopened.load().unwrap().expect("snap2 should be recovered");
    assert_eq!(recovered2, snap2);

    let part = recovered2.partition(PartitionId::new(10)).unwrap();
    let conv = part.conversion.as_ref().unwrap();
    assert_eq!(conv.generation, 2);
    assert_eq!(conv.from, StorageFormat::Row);
    assert_eq!(conv.to, StorageFormat::Column);
    assert_eq!(conv.snapshot_version, Version::new(42));
    assert_eq!(conv.phase, ConversionPhase::SegmentsWritten);

    let tab = recovered2.tablet(TabletId::new(100)).unwrap();
    let manifest = tab.column_manifest.as_ref().unwrap();
    assert_eq!(manifest.generation, 2);
    assert_eq!(manifest.path, "manifests/part_10_tablet_100.json");
    assert_eq!(manifest.base_version, Version::new(42));
    assert_eq!(manifest.segment_count, 5);
    assert_eq!(manifest.row_count, 250_000);

    // Step 2: Cut over to Column format at generation 3
    let mut snap3 = snap2.clone();
    snap3.generation = 3;
    snap3.partitions[0].generation = 3;
    snap3.partitions[0].storage = StorageDescriptor::Column;
    snap3.partitions[0].conversion = None;
    // Tablet keeps its column manifest reference from generation 2
    snap3.tablets[0].generation = 3;

    reopened.compare_and_set(2, snap3.clone()).unwrap();

    // Reopen and verify Column snapshot recovered cleanly
    drop(reopened);
    let reopened2 = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered3 = reopened2
        .load()
        .unwrap()
        .expect("snap3 should be recovered");
    assert_eq!(recovered3, snap3);
    assert_eq!(recovered3.partitions[0].storage, StorageDescriptor::Column);
    assert!(recovered3.partitions[0].conversion.is_none());
    assert_eq!(
        recovered3.tablets[0].column_manifest,
        snap2.tablets[0].column_manifest
    );
}

#[test]
fn test_invalid_conversion_combinations() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // 1. Storage is Row but conversion descriptor is present
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(10),
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("Row"));

    // 2. Storage is Column but conversion descriptor is present
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Column,
        StorageFormat::Row,
        Version::new(10),
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("Column"));

    // 3. Storage Converting generation != conversion generation
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        2, // mismatch: storage is 1
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(10),
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("does not match conversion generation"));

    // 4. Storage Converting direction != conversion direction
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Column, // mismatch: storage from is Row
        StorageFormat::Row,    // mismatch: storage to is Column
        Version::new(10),
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("direction"));

    // 5. Conversion generation is 0
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        0, // invalid: 0
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(10),
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("generation must be > 0"));

    // 6. Conversion from == to
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Row, // invalid
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Row,
        StorageFormat::Row, // invalid: from == to
        Version::new(10),
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("identical") || err.to_string().contains("differ"));

    // 7. Conversion snapshot version is 0 (invalid MVCC version)
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(0), // invalid: 0
        ConversionPhase::SnapshotPinned,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("snapshot version must be > 0"));

    // 8. Conversion to Row with SegmentsWritten phase
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Column,
        to: StorageFormat::Row,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Column,
        StorageFormat::Row,
        Version::new(10),
        ConversionPhase::SegmentsWritten, // invalid when converting to Row
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid when converting to Row"));
}

#[test]
fn test_invalid_column_manifest_and_paths() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // 1. Column manifest on Row partition
    let mut snap = make_valid_snapshot(1);
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("Row storage"));

    // 2. Column manifest on Converting partition with to == Row
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Column,
        to: StorageFormat::Row,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Column,
        StorageFormat::Row,
        Version::new(1),
        ConversionPhase::SnapshotPinned,
    ));
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("converting to Row"));

    // 3. Column manifest generation mismatch with conversion generation
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SegmentsWritten,
    ));
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        2, // mismatch: conversion is 1
        "manifests/tab.json",
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("does not match partition conversion generation"));

    // 4. Column manifest generation == 0
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        0, // invalid: 0
        "manifests/tab.json",
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("manifest generation must be > 0"));

    // 5. Column manifest base_version == 0
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(0), // invalid: 0
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("base_version must be > 0"));

    // 6. Column manifest empty path
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "   ", // empty path
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("path cannot be empty"));

    // 7. Column manifest absolute path
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "/etc/passwd", // absolute path
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("must be relative"));

    // 8. Column manifest path traversal with ..
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/../escape.json", // path traversal
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("cannot contain parent directory traversal"));

    // 9. Column manifest segment_count == 0 && row_count > 0
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(1),
        0,   // 0 segments
        100, // > 0 rows
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("row_count 100 > 0 but segment_count is 0"));

    // 10. Column manifest excessive segment_count
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(1),
        MAX_MANIFEST_SEGMENTS + 1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("segment_count"));

    // 11. Column manifest excessive row_count
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(1),
        1,
        MAX_MANIFEST_ROWS + 1,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("row_count"));

    // 12. Column manifest present when conversion phase is SnapshotPinned
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
    snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        1,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SnapshotPinned, // cannot have manifest at this phase
    ));
    snap.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        1,
        "manifests/tab.json",
        Version::new(1),
        1,
        100,
    ));
    let err = store.compare_and_set(0, snap).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("SnapshotPinned phase"));
}

#[test]
fn test_conversion_stale_cas() {
    let temp = TempDir::new().unwrap();
    let catalog_path = temp.path().join("CATALOG");
    let tmp_path = temp.path().join("CATALOG.tmp");

    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let snap1 = make_valid_snapshot(1);
    store.compare_and_set(0, snap1.clone()).unwrap();

    let bytes_gen1 = fs::read(&catalog_path).unwrap();

    // Stale expected generation CAS rejection
    let mut snap_converting = snap1.clone();
    snap_converting.generation = 2;
    snap_converting.partitions[0].generation = 2;
    snap_converting.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 2,
    };
    snap_converting.partitions[0].conversion = Some(ConversionDescriptor::new(
        2,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(10),
        ConversionPhase::SegmentsWritten,
    ));
    snap_converting.tablets[0].generation = 2;
    snap_converting.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        2,
        "manifests/t100.json",
        Version::new(10),
        1,
        500,
    ));

    // Present stale expected = 0 when current is 1
    let err = store
        .compare_and_set(0, snap_converting.clone())
        .unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));
    assert!(!tmp_path.exists());
    assert_eq!(fs::read(&catalog_path).unwrap(), bytes_gen1);

    // Present invalid CAS where target generation <= expected
    let mut snap_non_advancing = snap_converting.clone();
    snap_non_advancing.generation = 1; // not advancing
    let err = store.compare_and_set(1, snap_non_advancing).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(!tmp_path.exists());
    assert_eq!(fs::read(&catalog_path).unwrap(), bytes_gen1);

    // Present semantic error (e.g. invalid manifest path with ..)
    let mut snap_invalid_manifest = snap_converting.clone();
    snap_invalid_manifest.tablets[0].column_manifest = Some(ColumnManifestRef::new(
        2,
        "manifests/../../secret.json",
        Version::new(10),
        1,
        500,
    ));
    let err = store.compare_and_set(1, snap_invalid_manifest).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(!tmp_path.exists());
    assert_eq!(fs::read(&catalog_path).unwrap(), bytes_gen1);

    // Successful CAS to Converting gen 2
    store.compare_and_set(1, snap_converting.clone()).unwrap();
    assert_eq!(store.current_generation().unwrap(), 2);
    let bytes_gen2 = fs::read(&catalog_path).unwrap();
    assert_ne!(bytes_gen1, bytes_gen2);

    // Stale CAS attempt to cutover to Column presenting expected = 1 when current is 2
    let mut snap_column = snap_converting.clone();
    snap_column.generation = 3;
    snap_column.partitions[0].generation = 3;
    snap_column.partitions[0].storage = StorageDescriptor::Column;
    snap_column.partitions[0].conversion = None;
    snap_column.tablets[0].generation = 3;

    let err = store.compare_and_set(1, snap_column.clone()).unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));
    assert_eq!(store.current_generation().unwrap(), 2);
    assert_eq!(fs::read(&catalog_path).unwrap(), bytes_gen2);

    // Successful cutover to Column gen 3
    store.compare_and_set(2, snap_column.clone()).unwrap();
    assert_eq!(store.current_generation().unwrap(), 3);
    assert_eq!(store.load().unwrap().unwrap(), snap_column);
}

#[test]
fn test_partitioning_legacy_decode_and_reopen() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    // Legacy JSON format with no partitioning/range/list_values fields
    let legacy_json = r#"{
        "generation": 1,
        "tables": [{
            "id": 1,
            "name": "users",
            "schema": {
                "columns": [
                    {"name": "id", "data_type": "Int64", "nullable": false, "primary_key": true},
                    {"name": "val", "data_type": "String", "nullable": true, "primary_key": false}
                ]
            },
            "primary_key": [0],
            "partitions": [10],
            "generation": 1
        }],
        "partitions": [{
            "id": 10,
            "table_id": 1,
            "name": "p0",
            "storage": "Row",
            "tablets": [100],
            "generation": 1
        }],
        "tablets": [{
            "id": 100,
            "partition_id": 10,
            "bucket": 0,
            "replicas": [1000],
            "generation": 1
        }],
        "replicas": [{
            "id": 1000,
            "tablet_id": 100,
            "node_id": 42,
            "is_leader": true,
            "healthy": true,
            "generation": 1
        }]
    }"#;

    let snap: CatalogSnapshot = serde_json::from_str(legacy_json).unwrap();
    assert_eq!(snap.generation, 1);
    assert!(snap.tables[0].partitioning.is_none());
    assert!(snap.partitions[0].range.is_none());
    assert!(snap.partitions[0].list_values.is_empty());

    // Single-partition legacy descriptor validates cleanly
    snap.validate().unwrap();

    // Store, CAS, and reopen
    store.compare_and_set(0, snap.clone()).unwrap();
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered = reopened.load().unwrap().unwrap();
    assert_eq!(recovered, snap);

    // Unpartitioned table routes any value to its sole partition
    assert_eq!(
        recovered
            .route_partition_value(TableId::new(1), &Value::Int64(42))
            .unwrap(),
        PartitionId::new(10)
    );
    assert_eq!(
        recovered.tables[0]
            .route_partition_value(&recovered, &Value::String("anything".into()))
            .unwrap(),
        PartitionId::new(10)
    );
}

#[test]
fn test_range_partitioning_routing_and_boundaries() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".to_string(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();

    let p0_id = PartitionId::new(10);
    let p1_id = PartitionId::new(11);
    let p2_id = PartitionId::new(12);

    let table = TableDescriptor::new(
        TableId::new(1),
        "events",
        schema,
        vec![0],
        vec![p0_id, p1_id, p2_id],
        1,
    )
    .with_partitioning(PartitioningDescriptor::new(0, PartitioningMethod::Range));

    let p0 = PartitionDescriptor::new(
        p0_id,
        TableId::new(1),
        "p0",
        StorageDescriptor::Row,
        vec![TabletId::new(100)],
        1,
    )
    .with_range(RangeBound::new(Value::Int64(0), Value::Int64(100)));

    let p1 = PartitionDescriptor::new(
        p1_id,
        TableId::new(1),
        "p1",
        StorageDescriptor::Row,
        vec![TabletId::new(101)],
        1,
    )
    .with_range(RangeBound::new(Value::Int64(100), Value::Int64(200)));

    let p2 = PartitionDescriptor::new(
        p2_id,
        TableId::new(1),
        "p2",
        StorageDescriptor::Row,
        vec![TabletId::new(102)],
        1,
    )
    .with_range(RangeBound::new(Value::Int64(200), Value::Int64(300)));

    let tabs = vec![
        TabletDescriptor::new(TabletId::new(100), p0_id, 0, vec![ReplicaId::new(1000)], 1),
        TabletDescriptor::new(TabletId::new(101), p1_id, 0, vec![ReplicaId::new(1001)], 1),
        TabletDescriptor::new(TabletId::new(102), p2_id, 0, vec![ReplicaId::new(1002)], 1),
    ];

    let reps = vec![
        ReplicaDescriptor::new(
            ReplicaId::new(1000),
            TabletId::new(100),
            NodeId::new(1),
            true,
            true,
            1,
        ),
        ReplicaDescriptor::new(
            ReplicaId::new(1001),
            TabletId::new(101),
            NodeId::new(2),
            true,
            true,
            1,
        ),
        ReplicaDescriptor::new(
            ReplicaId::new(1002),
            TabletId::new(102),
            NodeId::new(3),
            true,
            true,
            1,
        ),
    ];

    let parts = vec![p0.clone(), p1.clone(), p2.clone()];
    let snap = CatalogSnapshot::new(1, vec![table.clone()], parts.clone(), tabs, reps);
    snap.validate().unwrap();

    // Verify boundaries: lower inclusive, upper exclusive
    // p0: [0, 100)
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(0))
            .unwrap(),
        p0_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(50))
            .unwrap(),
        p0_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(99))
            .unwrap(),
        p0_id
    );

    // p1: [100, 200) -> 100 must route to p1, not p0
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(100))
            .unwrap(),
        p1_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(150))
            .unwrap(),
        p1_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(199))
            .unwrap(),
        p1_id
    );

    // p2: [200, 300) -> 200 must route to p2, not p1
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(200))
            .unwrap(),
        p2_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(1), &Value::Int64(299))
            .unwrap(),
        p2_id
    );

    // Out-of-range / unmatched values return InvalidArgument
    let err = snap
        .route_partition_value(TableId::new(1), &Value::Int64(300))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    let err = snap
        .route_partition_value(TableId::new(1), &Value::Int64(-1))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    let err = snap
        .route_partition_value(TableId::new(1), &Value::Int64(999))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // TableDescriptor routing helper
    assert_eq!(
        table
            .route_partition_value(&snap, &Value::Int64(100))
            .unwrap(),
        p1_id
    );
    assert_eq!(
        table
            .route_partition_value(&parts, &Value::Int64(250))
            .unwrap(),
        p2_id
    );
    assert_eq!(
        snap.route_partition_value(&table, &Value::Int64(0))
            .unwrap(),
        p0_id
    );

    // CAS and reopen
    store.compare_and_set(0, snap.clone()).unwrap();
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered = reopened.load().unwrap().unwrap();
    assert_eq!(recovered, snap);
    assert_eq!(
        recovered
            .route_partition_value(TableId::new(1), &Value::Int64(100))
            .unwrap(),
        p1_id
    );
}

#[test]
fn test_list_partitioning_routing() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "region".to_string(),
            data_type: DataType::String,
            nullable: false,
            primary_key: true,
        },
    ])
    .unwrap();

    let p_east_id = PartitionId::new(20);
    let p_west_id = PartitionId::new(21);

    let table = TableDescriptor::new(
        TableId::new(2),
        "accounts",
        schema,
        vec![0, 1],
        vec![p_east_id, p_west_id],
        1,
    )
    .with_partitioning(PartitioningDescriptor::new(1, PartitioningMethod::List));

    let p_east = PartitionDescriptor::new(
        p_east_id,
        TableId::new(2),
        "p_east",
        StorageDescriptor::Row,
        vec![TabletId::new(200)],
        1,
    )
    .with_list_values(vec![
        Value::String("us-east-1".into()),
        Value::String("us-east-2".into()),
    ]);

    let p_west = PartitionDescriptor::new(
        p_west_id,
        TableId::new(2),
        "p_west",
        StorageDescriptor::Row,
        vec![TabletId::new(201)],
        1,
    )
    .with_list_values(vec![
        Value::String("us-west-1".into()),
        Value::String("us-west-2".into()),
    ]);

    let tabs = vec![
        TabletDescriptor::new(
            TabletId::new(200),
            p_east_id,
            0,
            vec![ReplicaId::new(2000)],
            1,
        ),
        TabletDescriptor::new(
            TabletId::new(201),
            p_west_id,
            0,
            vec![ReplicaId::new(2001)],
            1,
        ),
    ];
    let reps = vec![
        ReplicaDescriptor::new(
            ReplicaId::new(2000),
            TabletId::new(200),
            NodeId::new(1),
            true,
            true,
            1,
        ),
        ReplicaDescriptor::new(
            ReplicaId::new(2001),
            TabletId::new(201),
            NodeId::new(2),
            true,
            true,
            1,
        ),
    ];

    let snap = CatalogSnapshot::new(1, vec![table], vec![p_east, p_west], tabs, reps);
    snap.validate().unwrap();

    // Exact matching
    assert_eq!(
        snap.route_partition_value(TableId::new(2), &Value::String("us-east-1".into()))
            .unwrap(),
        p_east_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(2), &Value::String("us-east-2".into()))
            .unwrap(),
        p_east_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(2), &Value::String("us-west-1".into()))
            .unwrap(),
        p_west_id
    );
    assert_eq!(
        snap.route_partition_value(TableId::new(2), &Value::String("us-west-2".into()))
            .unwrap(),
        p_west_id
    );

    // Unmatched
    let err = snap
        .route_partition_value(TableId::new(2), &Value::String("eu-central-1".into()))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    let err = snap
        .route_partition_value(TableId::new(2), &Value::String("".into()))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // CAS and reopen
    store.compare_and_set(0, snap.clone()).unwrap();
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered = reopened.load().unwrap().unwrap();
    assert_eq!(recovered, snap);
}

#[test]
fn test_partitioning_duplicate_violations() {
    let schema = Schema::new(vec![ColumnDef {
        name: "code".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    // 1. Duplicate list value in the same partition
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].schema = schema.clone();
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));
    snap.partitions[0].list_values = vec![Value::Int64(10), Value::Int64(10)];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate list value"));

    // 2. Duplicate list value across different partitions in the same table
    let mut snap2 = make_valid_snapshot(1);
    snap2.tables[0].schema = schema.clone();
    snap2.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));
    snap2.partitions[0].list_values = vec![Value::Int64(10), Value::Int64(20)];

    let p2_id = PartitionId::new(11);
    let t2_id = TabletId::new(101);
    let r2_id = ReplicaId::new(1001);
    snap2.tables[0].partitions.push(p2_id);
    snap2.partitions.push(
        PartitionDescriptor::new(
            p2_id,
            TableId::new(1),
            "p1",
            StorageDescriptor::Row,
            vec![t2_id],
            1,
        )
        .with_list_values(vec![Value::Int64(20), Value::Int64(30)]), // 20 is duplicate
    );
    snap2
        .tablets
        .push(TabletDescriptor::new(t2_id, p2_id, 0, vec![r2_id], 1));
    snap2.replicas.push(ReplicaDescriptor::new(
        r2_id,
        t2_id,
        NodeId::new(2),
        true,
        true,
        1,
    ));
    let err = snap2.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("duplicate list value"));
}

#[test]
fn test_range_overlap_and_order_violations() {
    let schema = Schema::new(vec![ColumnDef {
        name: "id".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    // Helper to build a 2-partition range snapshot
    let build_range_snap = |r1: RangeBound, r2: RangeBound| {
        let p1_id = PartitionId::new(10);
        let p2_id = PartitionId::new(11);
        let t1_id = TabletId::new(100);
        let t2_id = TabletId::new(101);
        let r1_id = ReplicaId::new(1000);
        let r2_id = ReplicaId::new(1001);

        let table = TableDescriptor::new(
            TableId::new(1),
            "t",
            schema.clone(),
            vec![0],
            vec![p1_id, p2_id],
            1,
        )
        .with_partitioning(PartitioningDescriptor::new(0, PartitioningMethod::Range));

        let p1 = PartitionDescriptor::new(
            p1_id,
            TableId::new(1),
            "p1",
            StorageDescriptor::Row,
            vec![t1_id],
            1,
        )
        .with_range(r1);
        let p2 = PartitionDescriptor::new(
            p2_id,
            TableId::new(1),
            "p2",
            StorageDescriptor::Row,
            vec![t2_id],
            1,
        )
        .with_range(r2);

        let tablets = vec![
            TabletDescriptor::new(t1_id, p1_id, 0, vec![r1_id], 1),
            TabletDescriptor::new(t2_id, p2_id, 0, vec![r2_id], 1),
        ];
        let replicas = vec![
            ReplicaDescriptor::new(r1_id, t1_id, NodeId::new(1), true, true, 1),
            ReplicaDescriptor::new(r2_id, t2_id, NodeId::new(2), true, true, 1),
        ];

        CatalogSnapshot::new(1, vec![table], vec![p1, p2], tablets, replicas)
    };

    // 1. Overlap: [0, 100) and [50, 150)
    let snap = build_range_snap(
        RangeBound::new(Value::Int64(0), Value::Int64(100)),
        RangeBound::new(Value::Int64(50), Value::Int64(150)),
    );
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("overlapping range"));

    // 2. Identical: [0, 100) and [0, 100)
    let snap = build_range_snap(
        RangeBound::new(Value::Int64(0), Value::Int64(100)),
        RangeBound::new(Value::Int64(0), Value::Int64(100)),
    );
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("overlapping range"));

    // 3. Nested: [0, 100) and [20, 80)
    let snap = build_range_snap(
        RangeBound::new(Value::Int64(0), Value::Int64(100)),
        RangeBound::new(Value::Int64(20), Value::Int64(80)),
    );
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("overlapping range"));

    // 4. Inverted range order: lower > upper ([100, 50))
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].schema = schema.clone();
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(100), Value::Int64(50)));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("strictly less than upper"));

    // 5. Empty range: lower == upper ([100, 100))
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(100), Value::Int64(100)));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("strictly less than upper"));
}

#[test]
fn test_partitioning_type_and_null_violations() {
    // 1. Partition key column nullable in schema
    let schema_nullable = Schema::new(vec![ColumnDef {
        name: "id".to_string(),
        data_type: DataType::Int64,
        nullable: true, // nullable!
        primary_key: true,
    }])
    .unwrap();
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].schema = schema_nullable;
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(0), Value::Int64(100)));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("cannot be nullable"));

    // 2. Partition key column index out of bounds
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(99, PartitioningMethod::Range));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("out of bounds"));

    // 3. Partition key column not part of primary key
    let mut snap = make_valid_snapshot(1);
    // column 1 is 'val' with primary_key: false
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(1, PartitioningMethod::Range));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("must be part of primary key"));

    // 4. Range lower bound type mismatch
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(
        Value::String("0".into()),
        Value::Int64(100),
    ));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid type"));

    // 5. Range upper bound type mismatch
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(
        Value::Int64(0),
        Value::String("100".into()),
    ));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid type"));

    // 6. Range bound is Null
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(Value::Null, Value::Int64(100)));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid type"));

    // 7. List value type mismatch
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));
    snap.partitions[0].list_values = vec![Value::String("bad".into())];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid type"));

    // 8. List value is Null
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));
    snap.partitions[0].list_values = vec![Value::Null];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("invalid type"));

    // 9. Routing with wrong value type
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(0), Value::Int64(100)));
    snap.validate().unwrap();

    let err = snap
        .route_partition_value(TableId::new(1), &Value::String("wrong".into()))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("type mismatch"));

    // 10. Routing with Null value on partitioned table
    let err = snap
        .route_partition_value(TableId::new(1), &Value::Null)
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("cannot be null"));
}

#[test]
fn test_partitioning_ownership_and_method_consistency() {
    // 1. Range table has partition missing range bound
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = None;
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("missing range bound"));

    // 2. Range table has partition with list values
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(0), Value::Int64(100)));
    snap.partitions[0].list_values = vec![Value::Int64(50)];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("defines both range bound and list values"));

    // 3. List table has partition with range bound
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(0), Value::Int64(100)));
    snap.partitions[0].list_values = vec![Value::Int64(50)];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("defines both range bound and list values"));

    // 4. List table has partition with empty list values
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::List));
    snap.partitions[0].list_values = vec![];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("empty list values"));

    // 5. Unpartitioned table has partition with range bound
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].range = Some(RangeBound::new(Value::Int64(0), Value::Int64(100)));
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("parent table 'users' is unpartitioned"));

    // 6. Unpartitioned table has partition with list values
    let mut snap = make_valid_snapshot(1);
    snap.partitions[0].list_values = vec![Value::Int64(50)];
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("parent table 'users' is unpartitioned"));

    // 7. Partitioned table with 0 partitions
    let mut snap = make_valid_snapshot(1);
    snap.tables[0].partitioning = Some(PartitioningDescriptor::new(0, PartitioningMethod::Range));
    snap.tables[0].partitions.clear();
    snap.partitions.clear();
    snap.tablets.clear();
    snap.replicas.clear();
    let err = snap.validate().unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("must have at least one partition"));

    // 8. Routing non-existent table ID
    let valid_snap = make_valid_snapshot(1);
    let err = valid_snap
        .route_partition_value(TableId::new(999), &Value::Int64(0))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("not found"));

    // 9. Unpartitioned table with 2 partitions fails routing
    let mut snap = make_valid_snapshot(1);
    let p2_id = PartitionId::new(11);
    let t2_id = TabletId::new(101);
    let r2_id = ReplicaId::new(1001);
    snap.tables[0].partitions.push(p2_id);
    snap.partitions.push(PartitionDescriptor::new(
        p2_id,
        TableId::new(1),
        "p1",
        StorageDescriptor::Row,
        vec![t2_id],
        1,
    ));
    snap.tablets
        .push(TabletDescriptor::new(t2_id, p2_id, 0, vec![r2_id], 1));
    snap.replicas.push(ReplicaDescriptor::new(
        r2_id,
        t2_id,
        NodeId::new(2),
        true,
        true,
        1,
    ));
    snap.validate().unwrap(); // Validates structurally

    let err = snap
        .route_partition_value(TableId::new(1), &Value::Int64(42))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err
        .to_string()
        .contains("must have exactly one partition to route"));
}

#[test]
fn test_partitioning_cas_and_reopen_lifecycle() {
    let temp = TempDir::new().unwrap();
    let store = LocalCatalogStore::open(temp.path()).unwrap();

    let schema = Schema::new(vec![ColumnDef {
        name: "id".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    // Gen 1: Range-partitioned table with 1 partition [0, 100)
    let p0_id = PartitionId::new(10);
    let t0_id = TabletId::new(100);
    let r0_id = ReplicaId::new(1000);
    let table = TableDescriptor::new(
        TableId::new(1),
        "data",
        schema.clone(),
        vec![0],
        vec![p0_id],
        1,
    )
    .with_partitioning(PartitioningDescriptor::new(0, PartitioningMethod::Range));

    let p0 = PartitionDescriptor::new(
        p0_id,
        TableId::new(1),
        "p0",
        StorageDescriptor::Row,
        vec![t0_id],
        1,
    )
    .with_range(RangeBound::new(Value::Int64(0), Value::Int64(100)));
    let t0 = TabletDescriptor::new(t0_id, p0_id, 0, vec![r0_id], 1);
    let r0 = ReplicaDescriptor::new(r0_id, t0_id, NodeId::new(1), true, true, 1);

    let snap1 = CatalogSnapshot::new(1, vec![table], vec![p0], vec![t0], vec![r0]);
    store.compare_and_set(0, snap1.clone()).unwrap();

    // Gen 2: Add partition p1 [100, 200)
    let p1_id = PartitionId::new(11);
    let t1_id = TabletId::new(101);
    let r1_id = ReplicaId::new(1001);

    let mut snap2 = snap1.clone();
    snap2.generation = 2;
    snap2.tables[0].generation = 2;
    snap2.tables[0].partitions.push(p1_id);
    snap2.partitions.push(
        PartitionDescriptor::new(
            p1_id,
            TableId::new(1),
            "p1",
            StorageDescriptor::Row,
            vec![t1_id],
            2,
        )
        .with_range(RangeBound::new(Value::Int64(100), Value::Int64(200))),
    );
    snap2
        .tablets
        .push(TabletDescriptor::new(t1_id, p1_id, 0, vec![r1_id], 2));
    snap2.replicas.push(ReplicaDescriptor::new(
        r1_id,
        t1_id,
        NodeId::new(2),
        true,
        true,
        2,
    ));

    // Stale CAS expected = 0 rejects
    let err = store.compare_and_set(0, snap2.clone()).unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));

    // Valid CAS 1 -> 2
    store.compare_and_set(1, snap2).unwrap();
    assert_eq!(store.current_generation().unwrap(), 2);

    // Reopen and check routing across both partitions
    let reopened = LocalCatalogStore::open(temp.path()).unwrap();
    let recovered = reopened.load().unwrap().unwrap();
    assert_eq!(recovered.generation, 2);
    assert_eq!(
        recovered
            .route_partition_value(TableId::new(1), &Value::Int64(50))
            .unwrap(),
        p0_id
    );
    assert_eq!(
        recovered
            .route_partition_value(TableId::new(1), &Value::Int64(150))
            .unwrap(),
        p1_id
    );
}
