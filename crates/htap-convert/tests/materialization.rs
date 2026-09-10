use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, ConversionDescriptor, ConversionPhase, NodeId, PartitionDescriptor,
    PartitionId, ReplicaDescriptor, ReplicaId, StorageDescriptor, StorageFormat, TableDescriptor,
    TableId, TabletDescriptor, TabletId,
};
use htap_colstore::SegmentOptions;
use htap_common::{ColumnDef, DataType, HtapError, Row, Schema, Value, Version};
use htap_convert::{manifest_path, write_segment, LocalConverter, TabletColumnManifest};
use htap_rowstore::{Engine, EngineOptions, Mutation};
use tempfile::tempdir;

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

fn make_test_setup(
    dir: &Path,
) -> (
    Arc<LocalCatalogStore>,
    Arc<Engine>,
    TableId,
    PartitionId,
    TabletId,
    Schema,
) {
    let cat_dir = dir.join("catalog");
    let row_dir = dir.join("rowstore");

    let cat_store = Arc::new(LocalCatalogStore::open(cat_dir).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(row_dir)).unwrap());
    let schema = test_schema();

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);

    let table = TableDescriptor::new(
        table_id,
        "test_table",
        schema.clone(),
        vec![0],
        vec![part_id],
        1,
    );
    let partition = PartitionDescriptor::new(
        part_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(tablet_id, part_id, 0, vec![replica_id], 1);
    let replica = ReplicaDescriptor::new(replica_id, tablet_id, NodeId::new(1), true, true, 1);

    let snap = CatalogSnapshot::new(1, vec![table], vec![partition], vec![tablet], vec![replica]);
    cat_store.compare_and_set(0, snap).unwrap();

    (cat_store, engine, table_id, part_id, tablet_id, schema)
}

fn commit_put(engine: &Engine, partition_id: u64, id: i64, val: Option<&str>) -> Version {
    let key = htap_common::encode_key(&[Value::Int64(id)]).unwrap();
    let row = Row::new(vec![
        Value::Int64(id),
        val.map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
    ]);
    let snap = engine.snapshot();
    let txn_id = engine.committed_version().get() + 1;
    engine
        .commit(
            txn_id,
            snap,
            vec![Mutation::Put {
                partition_id,
                key,
                row,
            }],
        )
        .unwrap()
}

fn commit_delete(engine: &Engine, partition_id: u64, id: i64) -> Version {
    let key = htap_common::encode_key(&[Value::Int64(id)]).unwrap();
    let snap = engine.snapshot();
    let txn_id = engine.committed_version().get() + 1;
    engine
        .commit(txn_id, snap, vec![Mutation::Delete { partition_id, key }])
        .unwrap()
}

#[test]
fn test_simple_conversion() {
    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    // Commit 5 rows into rowstore
    for i in 1..=5 {
        commit_put(&engine, part_id.as_u64(), i, Some(&format!("item_{i}")));
    }

    let options = SegmentOptions::new().with_rows_per_block(2);
    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        options.clone(),
    );

    // Initial read in Row mode before conversion
    let rows_before = converter
        .read_column_partition(part_id, engine.snapshot())
        .unwrap();
    assert_eq!(rows_before.len(), 5);

    // Run conversion
    let manifest = converter.convert_partition(part_id).unwrap();
    assert_eq!(manifest.tablet_id, tablet_id);
    assert_eq!(manifest.total_rows(), 5);
    assert_eq!(manifest.segment_count(), 1);

    // Verify manifest file exists on disk
    let m_file = manifest_path(&colstore_dir, tablet_id);
    assert!(m_file.is_file());

    // Verify catalog snapshot was updated to Column
    let snap = cat_store.load().unwrap().unwrap();
    assert_eq!(snap.generation, 5); // 1 (init) -> 2 (SnapshotPinned) -> 3 (SegmentsWritten) -> 4 (ReadyToPublish) -> 5 (Column)

    let part_desc = snap.partition(part_id).unwrap();
    assert_eq!(part_desc.storage, StorageDescriptor::Column);
    assert!(part_desc.conversion.is_none());

    let tab_desc = snap.tablet(tablet_id).unwrap();
    let manifest_ref = tab_desc.column_manifest.as_ref().unwrap();
    assert_eq!(manifest_ref.row_count, 5);
    assert_eq!(manifest_ref.segment_count, 1);
    assert_eq!(manifest_ref.base_version, manifest.base_version);

    // Read via read_column_partition after conversion
    let rows_after = converter
        .read_column_partition(part_id, engine.snapshot())
        .unwrap();
    assert_eq!(rows_after.len(), 5);
    for (i, row) in rows_after.iter().enumerate() {
        let expected_id = (i + 1) as i64;
        assert_eq!(row.get(0), Some(&Value::Int64(expected_id)));
        assert_eq!(
            row.get(1),
            Some(&Value::String(format!("item_{expected_id}")))
        );
    }

    // Calling convert_partition again is idempotent and returns the published manifest
    let manifest2 = converter.convert_partition(part_id).unwrap();
    assert_eq!(manifest2, manifest);
}

#[test]
fn test_post_conversion_put_delete_overlay() {
    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, _tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    // Commit 4 rows: 1, 2, 3, 4
    commit_put(&engine, part_id.as_u64(), 1, Some("one"));
    commit_put(&engine, part_id.as_u64(), 2, Some("two"));
    commit_put(&engine, part_id.as_u64(), 3, Some("three"));
    commit_put(&engine, part_id.as_u64(), 4, Some("four"));

    let base_snap = engine.snapshot();

    // Convert partition to Column
    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );
    converter.convert_partition(part_id).unwrap();

    // Post-conversion mutations in rowstore:
    // 1. Update existing row 2: "two" -> "two_updated"
    commit_put(&engine, part_id.as_u64(), 2, Some("two_updated"));
    // 2. Delete existing row 3
    commit_delete(&engine, part_id.as_u64(), 3);
    // 3. Insert new row 5: "five"
    commit_put(&engine, part_id.as_u64(), 5, Some("five"));

    let current_snap = engine.snapshot();

    // Read with current snapshot: should overlay the post-conversion mutations
    let current_rows = converter
        .read_column_partition(part_id, current_snap)
        .unwrap();
    assert_eq!(current_rows.len(), 4); // rows 1, 2 (updated), 4, 5 (row 3 deleted)

    assert_eq!(current_rows[0].get(0), Some(&Value::Int64(1)));
    assert_eq!(current_rows[0].get(1), Some(&Value::String("one".into())));

    assert_eq!(current_rows[1].get(0), Some(&Value::Int64(2)));
    assert_eq!(
        current_rows[1].get(1),
        Some(&Value::String("two_updated".into()))
    );

    assert_eq!(current_rows[2].get(0), Some(&Value::Int64(4)));
    assert_eq!(current_rows[2].get(1), Some(&Value::String("four".into())));

    assert_eq!(current_rows[3].get(0), Some(&Value::Int64(5)));
    assert_eq!(current_rows[3].get(1), Some(&Value::String("five".into())));

    // Rowstore is authoritative: reading at the base snapshot before post-conversion
    // mutations should return the unmodified base state (rows 1, 2, 3, 4)
    let base_rows = converter.read_column_partition(part_id, base_snap).unwrap();
    assert_eq!(base_rows.len(), 4);
    assert_eq!(base_rows[1].get(1), Some(&Value::String("two".into())));
    assert_eq!(base_rows[2].get(0), Some(&Value::Int64(3)));
}

#[test]
fn test_history_reopen() {
    let dir = tempdir().unwrap();
    let cat_dir = dir.path().join("catalog");
    let row_dir = dir.path().join("rowstore");
    let col_dir = dir.path().join("colstore");

    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);

    // Initial session
    {
        let (cat_store, engine, _table_id, _part_id, _tablet_id, _schema) =
            make_test_setup(dir.path());
        commit_put(&engine, part_id.as_u64(), 10, Some("initial_10"));
        commit_put(&engine, part_id.as_u64(), 20, Some("initial_20"));

        let converter =
            LocalConverter::new(cat_store, engine.clone(), &col_dir, SegmentOptions::new());
        converter.convert_partition(part_id).unwrap();

        // Perform post-conversion write
        commit_put(&engine, part_id.as_u64(), 30, Some("post_30"));
        commit_delete(&engine, part_id.as_u64(), 10);
        // Explicitly flush engine
        engine.flush().unwrap();
    }

    // Reopen session
    {
        let cat_store_reopened = Arc::new(LocalCatalogStore::open(&cat_dir).unwrap());
        let engine_reopened = Arc::new(Engine::open(EngineOptions::new(&row_dir)).unwrap());

        // Verify catalog state is recovered
        let cat_snap = cat_store_reopened.load().unwrap().unwrap();
        let part_desc = cat_snap.partition(part_id).unwrap();
        assert_eq!(part_desc.storage, StorageDescriptor::Column);
        let tab_desc = cat_snap.tablet(tablet_id).unwrap();
        assert!(tab_desc.column_manifest.is_some());

        let converter = LocalConverter::new(
            cat_store_reopened,
            engine_reopened.clone(),
            &col_dir,
            SegmentOptions::new(),
        );

        let rows = converter.read_column_partition_current(part_id).unwrap();
        assert_eq!(rows.len(), 2); // 20 and 30 (10 was deleted)
        assert_eq!(rows[0].get(0), Some(&Value::Int64(20)));
        assert_eq!(rows[0].get(1), Some(&Value::String("initial_20".into())));
        assert_eq!(rows[1].get(0), Some(&Value::Int64(30)));
        assert_eq!(rows[1].get(1), Some(&Value::String("post_30".into())));
    }
}

#[test]
fn test_empty_partition_conversion_and_overlay() {
    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    // Partition has 0 rows committed initially.
    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    let manifest = converter.convert_partition(part_id).unwrap();
    assert_eq!(manifest.total_rows(), 0);
    assert_eq!(manifest.segment_count(), 0);

    // Read back empty partition
    let rows = converter
        .read_column_partition(part_id, engine.snapshot())
        .unwrap();
    assert!(rows.is_empty());

    // Post-conversion insert into rowstore
    commit_put(&engine, part_id.as_u64(), 100, Some("first_item"));

    let rows_after = converter
        .read_column_partition(part_id, engine.snapshot())
        .unwrap();
    assert_eq!(rows_after.len(), 1);
    assert_eq!(rows_after[0].get(0), Some(&Value::Int64(100)));
    assert_eq!(
        rows_after[0].get(1),
        Some(&Value::String("first_item".into()))
    );

    // Verify catalog has manifest ref
    let snap = cat_store.load().unwrap().unwrap();
    let tab = snap.tablet(tablet_id).unwrap();
    let m_ref = tab.column_manifest.as_ref().unwrap();
    assert_eq!(m_ref.row_count, 0);
    assert_eq!(m_ref.segment_count, 0);
}

#[test]
fn test_all_values_and_nulls() {
    let dir = tempdir().unwrap();
    let cat_store = Arc::new(LocalCatalogStore::open(dir.path().join("catalog")).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(dir.path().join("rowstore"))).unwrap());
    let colstore_dir = dir.path().join("colstore");

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "c_bool".into(),
            data_type: DataType::Bool,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_int32".into(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_float64".into(),
            data_type: DataType::Float64,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_string".into(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_bytes".into(),
            data_type: DataType::Bytes,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_timestamp".into(),
            data_type: DataType::Timestamp,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(1);
    let tablet_id = TabletId::new(1);
    let replica_id = ReplicaId::new(1);

    let table = TableDescriptor::new(
        table_id,
        "all_types_table",
        schema.clone(),
        vec![0],
        vec![part_id],
        1,
    );
    let partition = PartitionDescriptor::new(
        part_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(tablet_id, part_id, 0, vec![replica_id], 1);
    let replica = ReplicaDescriptor::new(replica_id, tablet_id, NodeId::new(1), true, true, 1);

    let snap = CatalogSnapshot::new(1, vec![table], vec![partition], vec![tablet], vec![replica]);
    cat_store.compare_and_set(0, snap).unwrap();

    // Prepare rows with various data combinations
    let test_rows = vec![
        // Row 1: All non-null values
        Row::new(vec![
            Value::Int64(1),
            Value::Bool(true),
            Value::Int32(42),
            Value::Float64(std::f64::consts::PI),
            Value::String("hello world".into()),
            Value::Bytes(b"binary data".to_vec()),
            Value::Timestamp(1_700_000_000),
        ]),
        // Row 2: All nullable columns are NULL
        Row::new(vec![
            Value::Int64(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        // Row 3: Mixed nulls and edge values (empty string, empty bytes, false, 0)
        Row::new(vec![
            Value::Int64(3),
            Value::Bool(false),
            Value::Int32(0),
            Value::Null,
            Value::String("".into()),
            Value::Bytes(vec![]),
            Value::Timestamp(0),
        ]),
        // Row 4: Extreme values
        Row::new(vec![
            Value::Int64(4),
            Value::Bool(true),
            Value::Int32(i32::MAX),
            Value::Float64(f64::MIN_POSITIVE),
            Value::String("nested \"quotes\" and \0 escapes".into()),
            Value::Bytes(vec![0x00, 0xff, 0x01]),
            Value::Timestamp(i64::MAX),
        ]),
    ];

    for row in &test_rows {
        let pk = htap_common::encode_key(&[row.get(0).unwrap().clone()]).unwrap();
        let s = engine.snapshot();
        let tid = engine.committed_version().get() + 1;
        engine
            .commit(
                tid,
                s,
                vec![Mutation::Put {
                    partition_id: part_id.as_u64(),
                    key: pk,
                    row: row.clone(),
                }],
            )
            .unwrap();
    }

    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new().with_rows_per_block(2),
    );

    converter.convert_partition(part_id).unwrap();

    let read_rows = converter.read_column_partition_current(part_id).unwrap();
    assert_eq!(read_rows.len(), 4);
    for (i, row) in read_rows.iter().enumerate() {
        assert_eq!(row, &test_rows[i]);
    }
}

#[test]
fn test_pinned_snapshot_and_reuse_persisted_pin() {
    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    // Commit rows 1 and 2 at versions 1 and 2
    commit_put(&engine, part_id.as_u64(), 1, Some("v1_val"));
    let pin_v = commit_put(&engine, part_id.as_u64(), 2, Some("v2_val"));

    // Manually transition catalog to Converting with SnapshotPinned at pin_v
    let cat_snap = cat_store.load().unwrap().unwrap();
    let next_gen = cat_snap.generation + 1;
    let mut converting_snap = cat_snap.clone();
    converting_snap.generation = next_gen;

    let part_mut = converting_snap
        .partitions
        .iter_mut()
        .find(|p| p.id == part_id)
        .unwrap();
    part_mut.generation = next_gen;
    part_mut.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: next_gen,
    };
    part_mut.conversion = Some(ConversionDescriptor::new(
        next_gen,
        StorageFormat::Row,
        StorageFormat::Column,
        pin_v,
        ConversionPhase::SnapshotPinned,
    ));

    cat_store
        .compare_and_set(cat_snap.generation, converting_snap)
        .unwrap();

    // Now commit additional rows AFTER the pin!
    commit_put(&engine, part_id.as_u64(), 3, Some("v3_val"));
    commit_put(&engine, part_id.as_u64(), 1, Some("v1_updated")); // update row 1

    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    // Running convert_partition MUST reuse the persisted pin (pin_v = version 2)
    let manifest = converter.convert_partition(part_id).unwrap();
    assert_eq!(manifest.base_version, pin_v);
    // Manifest row count is 2 (rows 1 and 2 as of version 2)
    assert_eq!(manifest.total_rows(), 2);

    // Reading at the pinned snapshot gives the state as of the pin (rows 1 and 2)
    let pin_rows = converter.read_column_partition(part_id, pin_v).unwrap();
    assert_eq!(pin_rows.len(), 2);
    assert_eq!(pin_rows[0].get(0), Some(&Value::Int64(1)));
    assert_eq!(
        pin_rows[0].get(1),
        Some(&Value::String("v1_val".into())) // NOT v1_updated
    );
    assert_eq!(pin_rows[1].get(0), Some(&Value::Int64(2)));
    assert_eq!(pin_rows[1].get(1), Some(&Value::String("v2_val".into())));

    // Reading at latest visible snapshot overlays the post-pin writes (rows 1 updated, 2, 3 added)
    let latest_rows = converter.read_column_partition_current(part_id).unwrap();
    assert_eq!(latest_rows.len(), 3);
    assert_eq!(latest_rows[0].get(0), Some(&Value::Int64(1)));
    assert_eq!(
        latest_rows[0].get(1),
        Some(&Value::String("v1_updated".into()))
    );
    assert_eq!(latest_rows[1].get(0), Some(&Value::Int64(2)));
    assert_eq!(latest_rows[1].get(1), Some(&Value::String("v2_val".into())));
    assert_eq!(latest_rows[2].get(0), Some(&Value::Int64(3)));
    assert_eq!(latest_rows[2].get(1), Some(&Value::String("v3_val".into())));

    // Verify catalog state is final Column
    let final_cat = cat_store.load().unwrap().unwrap();
    let final_part = final_cat.partition(part_id).unwrap();
    assert_eq!(final_part.storage, StorageDescriptor::Column);
    assert!(final_part.conversion.is_none());
    let final_tab = final_cat.tablet(tablet_id).unwrap();
    assert_eq!(
        final_tab.column_manifest.as_ref().unwrap().base_version,
        pin_v
    );
}

#[test]
fn test_final_catalog_visibility_and_transitions() {
    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    commit_put(&engine, part_id.as_u64(), 1, Some("a"));

    // Check catalog before
    let snap1 = cat_store.load().unwrap().unwrap();
    assert_eq!(snap1.generation, 1);
    assert_eq!(
        snap1.partition(part_id).unwrap().storage,
        StorageDescriptor::Row
    );
    assert!(snap1.partition(part_id).unwrap().conversion.is_none());
    assert!(snap1.tablet(tablet_id).unwrap().column_manifest.is_none());

    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    converter.convert_partition(part_id).unwrap();

    // Check catalog after
    let snap3 = cat_store.load().unwrap().unwrap();
    assert_eq!(snap3.generation, 5);

    let part = snap3.partition(part_id).unwrap();
    assert_eq!(part.storage, StorageDescriptor::Column);
    assert!(part.conversion.is_none());
    assert_eq!(part.generation, 5);

    let tab = snap3.tablet(tablet_id).unwrap();
    assert_eq!(tab.generation, 5);
    let mref = tab.column_manifest.as_ref().unwrap();
    assert_eq!(mref.generation, 2);
    assert_eq!(mref.segment_count, 1);
    assert_eq!(mref.row_count, 1);
    assert_eq!(mref.path, format!("tablet-{}/MANIFEST", tablet_id.as_u64()));

    // Verify catalog snapshot validates cleanly
    snap3.validate().expect("catalog snapshot must validate");
}

struct InterceptingCatalogStore {
    inner: Arc<LocalCatalogStore>,
    colstore_dir: PathBuf,
    tablet_id: TabletId,
    cas_observations: Mutex<Vec<(u64, ConversionPhase, bool)>>,
}

impl CatalogStore for InterceptingCatalogStore {
    fn load(&self) -> htap_common::Result<Option<CatalogSnapshot>> {
        self.inner.load()
    }

    fn compare_and_set(
        &self,
        expected_generation: u64,
        next: CatalogSnapshot,
    ) -> htap_common::Result<()> {
        let manifest_file = manifest_path(&self.colstore_dir, self.tablet_id);
        let manifest_exists = manifest_file.is_file();

        if let Some(part) = next
            .partitions
            .iter()
            .find(|p| p.tablets.contains(&self.tablet_id))
        {
            if let Some(conv) = &part.conversion {
                self.cas_observations.lock().unwrap().push((
                    next.generation,
                    conv.phase,
                    manifest_exists,
                ));
            }
        }

        self.inner.compare_and_set(expected_generation, next)
    }
}

#[test]
fn test_conversion_phase_cas_persistence_boundary() {
    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    commit_put(&engine, part_id.as_u64(), 1, Some("row1"));

    let interceptor = Arc::new(InterceptingCatalogStore {
        inner: cat_store.clone(),
        colstore_dir: colstore_dir.clone(),
        tablet_id,
        cas_observations: Mutex::new(Vec::new()),
    });

    let converter = LocalConverter::new(
        interceptor.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    let manifest = converter.convert_partition(part_id).unwrap();
    assert_eq!(manifest.tablet_id, tablet_id);
    assert_eq!(manifest.total_rows(), 1);

    let observations = interceptor.cas_observations.lock().unwrap().clone();
    // Must observe 3 persisted conversion phase CAS transitions:
    // 1. SnapshotPinned: manifest does not yet exist on disk
    // 2. SegmentsWritten: manifest and segments are durable on disk
    // 3. ReadyToPublish: manifest and segments are durable on disk
    assert_eq!(observations.len(), 3);

    assert_eq!(observations[0].0, 2);
    assert_eq!(observations[0].1, ConversionPhase::SnapshotPinned);
    assert!(
        !observations[0].2,
        "manifest must not exist before rowstore materialization"
    );

    assert_eq!(observations[1].0, 3);
    assert_eq!(observations[1].1, ConversionPhase::SegmentsWritten);
    assert!(
        observations[1].2,
        "manifest must be durable when SegmentsWritten is persisted"
    );

    assert_eq!(observations[2].0, 4);
    assert_eq!(observations[2].1, ConversionPhase::ReadyToPublish);
    assert!(
        observations[2].2,
        "manifest must be durable when ReadyToPublish is persisted"
    );

    // Final catalog state is Column format at generation 5
    let final_snap = cat_store.load().unwrap().unwrap();
    assert_eq!(final_snap.generation, 5);
    let part = final_snap.partition(part_id).unwrap();
    assert_eq!(part.storage, StorageDescriptor::Column);
    assert!(part.conversion.is_none());
}

struct MockCatalogStore {
    snapshot: Mutex<Option<CatalogSnapshot>>,
}

impl MockCatalogStore {
    fn new(snap: CatalogSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(Some(snap)),
        }
    }

    fn set(&self, snap: CatalogSnapshot) {
        *self.snapshot.lock().unwrap() = Some(snap);
    }
}

impl CatalogStore for MockCatalogStore {
    fn load(&self) -> htap_common::Result<Option<CatalogSnapshot>> {
        Ok(self.snapshot.lock().unwrap().clone())
    }

    fn compare_and_set(
        &self,
        expected_generation: u64,
        next: CatalogSnapshot,
    ) -> htap_common::Result<()> {
        let mut lock = self.snapshot.lock().unwrap();
        let cur_gen = lock.as_ref().map(|s| s.generation).unwrap_or(0);
        if expected_generation != cur_gen {
            return Err(HtapError::Conflict(format!(
                "generation mismatch: expected {expected_generation}, current {cur_gen}"
            )));
        }
        *lock = Some(next);
        Ok(())
    }
}

#[test]
fn test_conversion_topology_rejection() {
    let dir = tempdir().unwrap();
    let colstore_dir = dir.path().join("colstore");
    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);
    let schema = test_schema();
    let table = TableDescriptor::new(table_id, "test_table", schema, vec![0], vec![part_id], 1);
    let partition = PartitionDescriptor::new(
        part_id,
        table_id,
        "p0",
        StorageDescriptor::Row,
        vec![tablet_id],
        1,
    );
    let tablet = TabletDescriptor::new(tablet_id, part_id, 0, vec![replica_id], 1);
    let replica = ReplicaDescriptor::new(replica_id, tablet_id, NodeId::new(1), true, true, 1);

    let base_snap = CatalogSnapshot::new(
        1,
        vec![table.clone()],
        vec![partition.clone()],
        vec![tablet.clone()],
        vec![replica.clone()],
    );
    let mock_store = Arc::new(MockCatalogStore::new(base_snap));
    let engine = Arc::new(Engine::open(EngineOptions::new(dir.path().join("rowstore"))).unwrap());
    let converter = LocalConverter::new(
        mock_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    // 1. Partition with 0 tablets -> InvalidArgument
    {
        let mut snap = CatalogSnapshot::new(
            2,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tablet.clone()],
            vec![replica.clone()],
        );
        snap.partitions[0].tablets = vec![];
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 2. Partition with > 1 tablets -> Unsupported
    {
        let mut snap = CatalogSnapshot::new(
            3,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tablet.clone()],
            vec![replica.clone()],
        );
        snap.partitions[0].tablets = vec![tablet_id, TabletId::new(101)];
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::Unsupported(_)));
    }

    // 3. Tablet missing from catalog -> Internal
    {
        let snap = CatalogSnapshot::new(
            4,
            vec![table.clone()],
            vec![partition.clone()],
            vec![],
            vec![replica.clone()],
        );
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::Internal(_)));
    }

    // 4. Tablet with 0 replicas -> InvalidArgument
    {
        let mut tab_no_rep = tablet.clone();
        tab_no_rep.replicas = vec![];
        let snap = CatalogSnapshot::new(
            5,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tab_no_rep],
            vec![replica.clone()],
        );
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 5. Tablet with > 1 replicas -> Unsupported
    {
        let mut tab_multi_rep = tablet.clone();
        tab_multi_rep.replicas = vec![replica_id, ReplicaId::new(1001)];
        let snap = CatalogSnapshot::new(
            6,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tab_multi_rep],
            vec![replica.clone()],
        );
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::Unsupported(_)));
    }

    // 6. Replica missing from catalog -> Internal
    {
        let snap = CatalogSnapshot::new(
            7,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tablet.clone()],
            vec![],
        );
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::Internal(_)));
    }

    // 7. Replica unhealthy -> Internal
    {
        let mut r_unhealthy = replica.clone();
        r_unhealthy.healthy = false;
        let snap = CatalogSnapshot::new(
            8,
            vec![table.clone()],
            vec![partition.clone()],
            vec![tablet.clone()],
            vec![r_unhealthy],
        );
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::Internal(_)));
    }

    // 8. Replica not leader -> Internal
    {
        let mut r_not_leader = replica.clone();
        r_not_leader.is_leader = false;
        let snap = CatalogSnapshot::new(
            9,
            vec![table],
            vec![partition],
            vec![tablet],
            vec![r_not_leader],
        );
        mock_store.set(snap);

        let err = converter.convert_partition(part_id).unwrap_err();
        assert!(matches!(err, HtapError::Internal(_)));
    }
}

#[test]
fn test_retry_from_segments_written_and_ready_to_publish() {
    let dir = tempdir().unwrap();
    let colstore_dir = dir.path().join("colstore");
    let (cat_store, engine, _table_id, part_id, tablet_id, schema) = make_test_setup(dir.path());

    commit_put(&engine, part_id.as_u64(), 10, Some("ten"));
    let pin_v = engine.committed_version();

    // 1. Manually write segments and manifest to disk
    let seg_entry = write_segment(
        &colstore_dir,
        tablet_id,
        2,
        "seg-0.col",
        &schema,
        vec![Row::new(vec![
            Value::Int64(10),
            Value::String("ten".into()),
        ])],
        &SegmentOptions::new(),
    )
    .unwrap();

    let manifest = TabletColumnManifest::new(2, tablet_id, schema, pin_v, vec![seg_entry]);
    htap_convert::write_atomic(&colstore_dir, &manifest).unwrap();

    // Set catalog to SegmentsWritten
    let cat_snap = cat_store.load().unwrap().unwrap();
    let next_gen = cat_snap.generation + 1;
    let mut sw_snap = cat_snap.clone();
    sw_snap.generation = next_gen;
    sw_snap.partitions[0].generation = next_gen;
    sw_snap.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 2,
    };
    sw_snap.partitions[0].conversion = Some(ConversionDescriptor::new(
        2,
        StorageFormat::Row,
        StorageFormat::Column,
        pin_v,
        ConversionPhase::SegmentsWritten,
    ));
    cat_store
        .compare_and_set(cat_snap.generation, sw_snap)
        .unwrap();

    // Reopening/retrying from SegmentsWritten should complete through ReadyToPublish to Column
    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    let m = converter.convert_partition(part_id).unwrap();
    assert_eq!(m.tablet_id, tablet_id);
    assert_eq!(m.generation, 2);
    assert_eq!(m.base_version, pin_v);

    let final_cat = cat_store.load().unwrap().unwrap();
    assert_eq!(
        final_cat.partition(part_id).unwrap().storage,
        StorageDescriptor::Column
    );
    assert!(final_cat
        .tablet(tablet_id)
        .unwrap()
        .column_manifest
        .is_some());

    // Repeating call is idempotent
    let m2 = converter.convert_partition(part_id).unwrap();
    assert_eq!(m2, m);
}

#[test]
fn test_convert_partition_generation_overflow_no_catalog_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, _tablet_id, _schema) = make_test_setup(tmp.path());
    let colstore_dir = tmp.path().join("colstore");

    // Set catalog generation to u64::MAX
    let snap = cat_store.load().unwrap().unwrap();
    let mut snap_max = snap.clone();
    snap_max.generation = u64::MAX;
    cat_store
        .compare_and_set(snap.generation, snap_max)
        .unwrap();

    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new(),
    );

    let err = converter.convert_partition(part_id).unwrap_err();
    assert!(matches!(
        err,
        HtapError::CounterOverflow {
            counter: "catalog_generation"
        }
    ));

    // Verify catalog generation and partition state were not mutated
    let final_cat = cat_store.load().unwrap().unwrap();
    assert_eq!(final_cat.generation, u64::MAX);
    assert_eq!(
        final_cat.partition(part_id).unwrap().storage,
        StorageDescriptor::Row
    );
}

#[test]
fn test_snapshot_aware_core_read() {
    use htap_convert::read_column_partition_core;

    let dir = tempdir().unwrap();
    let (cat_store, engine, _table_id, part_id, tablet_id, _schema) = make_test_setup(dir.path());
    let colstore_dir = dir.path().join("colstore");

    // Commit 3 initial rows in Row format
    commit_put(&engine, part_id.as_u64(), 10, Some("val_10"));
    commit_put(&engine, part_id.as_u64(), 20, Some("val_20"));
    commit_put(&engine, part_id.as_u64(), 30, Some("val_30"));

    let snap_v1 = engine.snapshot();

    // 1. Core read with Row partition
    let cat_snap1 = cat_store.load().unwrap().unwrap();
    let rows_row =
        read_column_partition_core(&cat_snap1, &engine, &colstore_dir, part_id, snap_v1).unwrap();
    assert_eq!(rows_row.len(), 3);
    assert_eq!(rows_row[0].get(0), Some(&Value::Int64(10)));
    assert_eq!(rows_row[1].get(0), Some(&Value::Int64(20)));
    assert_eq!(rows_row[2].get(0), Some(&Value::Int64(30)));

    // 2. Convert to Column
    let converter = LocalConverter::new(
        cat_store.clone(),
        engine.clone(),
        &colstore_dir,
        SegmentOptions::new().with_rows_per_block(2),
    );
    let manifest = converter.convert_partition(part_id).unwrap();
    assert_eq!(manifest.tablet_id, tablet_id);
    let base_snap = engine.snapshot();

    // 3. Commit mutations after base version:
    // Update key 20, Delete key 10, Insert key 25
    commit_put(&engine, part_id.as_u64(), 20, Some("val_20_updated"));
    commit_delete(&engine, part_id.as_u64(), 10);
    commit_put(&engine, part_id.as_u64(), 25, Some("val_25_new"));

    let latest_snap = engine.snapshot();

    let cat_snap2 = cat_store.load().unwrap().unwrap();
    assert_eq!(
        cat_snap2.partition(part_id).unwrap().storage,
        StorageDescriptor::Column
    );

    // 4. Historical read before base version falls back to rowstore
    let hist_rows =
        read_column_partition_core(&cat_snap2, &engine, &colstore_dir, part_id, snap_v1).unwrap();
    assert_eq!(hist_rows.len(), 3);
    assert_eq!(hist_rows[0].get(0), Some(&Value::Int64(10)));
    assert_eq!(hist_rows[1].get(0), Some(&Value::Int64(20)));
    assert_eq!(
        hist_rows[1].get(1),
        Some(&Value::String("val_20".to_string()))
    );
    assert_eq!(hist_rows[2].get(0), Some(&Value::Int64(30)));

    // 5. Read at base snapshot: columnar base rows (10, 20, 30)
    let base_rows =
        read_column_partition_core(&cat_snap2, &engine, &colstore_dir, part_id, base_snap).unwrap();
    assert_eq!(base_rows.len(), 3);
    assert_eq!(base_rows[0].get(0), Some(&Value::Int64(10)));
    assert_eq!(base_rows[1].get(0), Some(&Value::Int64(20)));
    assert_eq!(base_rows[2].get(0), Some(&Value::Int64(30)));

    // 6. Read at latest snapshot: base segment + delta overlay (key 10 deleted, key 20 updated, key 25 added, key 30 intact)
    // Deterministic PK order: 20, 25, 30
    let latest_rows =
        read_column_partition_core(&cat_snap2, &engine, &colstore_dir, part_id, latest_snap)
            .unwrap();
    assert_eq!(latest_rows.len(), 3);
    assert_eq!(latest_rows[0].get(0), Some(&Value::Int64(20)));
    assert_eq!(
        latest_rows[0].get(1),
        Some(&Value::String("val_20_updated".to_string()))
    );
    assert_eq!(latest_rows[1].get(0), Some(&Value::Int64(25)));
    assert_eq!(
        latest_rows[1].get(1),
        Some(&Value::String("val_25_new".to_string()))
    );
    assert_eq!(latest_rows[2].get(0), Some(&Value::Int64(30)));
    assert_eq!(
        latest_rows[2].get(1),
        Some(&Value::String("val_30".to_string()))
    );
}
