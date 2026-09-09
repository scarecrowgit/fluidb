//! Tests for catalog validation, serialization, crash safety, and recovery.

use std::fs;

use htap_catalog::local::{encode_snapshot, FORMAT_VERSION, HEADER_MAGIC};
use htap_catalog::*;
use htap_common::{ColumnDef, DataType, HtapError, Schema};
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

    // 3. Schema with empty column name
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

    // 4. Converting storage descriptor roundtrip
    let mut snap_converting = make_valid_snapshot(1);
    snap_converting.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };
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
