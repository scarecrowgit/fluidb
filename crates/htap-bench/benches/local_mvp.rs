//! Benchmark suite for local MVP storage, columnar scan, conversion, movement, and coordination.

use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_colstore::{Predicate, ScanRequest, SegmentOptions, SegmentReader, SegmentWriter};
use htap_common::{encode_key, ColumnDef, DataType, Mutation, Row, Schema, Value};
use htap_convert::LocalConverter;
use htap_coord::placement::plan_placement;
use htap_coord::{Coordinator, LocalCoordinator};
use htap_movement::{CopyOptions, DataFormat, LocalDataMover};
use htap_rowstore::{Engine, EngineOptions, Snapshot};
use htap_txn::{ParticipantId, RowstoreParticipant, TransactionManager};
use tempfile::TempDir;

/// Modest explicit Criterion configuration for fast, deterministic local benchmark execution.
fn custom_criterion() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1))
        .sample_size(10)
}

// -----------------------------------------------------------------------------
// 1. Rowstore: point get at stable snapshot
// -----------------------------------------------------------------------------

fn bench_rowstore_point_get(c: &mut Criterion) {
    let dir = TempDir::new().expect("tempdir");
    let engine = Engine::open(EngineOptions::new(dir.path())).expect("open rowstore engine");

    const NUM_ROWS: i64 = 1000;
    const TARGET_ID: i64 = 420;
    const PARTITION_ID: u64 = 0;

    let mut mutations = Vec::with_capacity(NUM_ROWS as usize);
    for i in 0..NUM_ROWS {
        let key = format!("key_{i:06}").into_bytes();
        let row = Row::new(vec![Value::Int64(i), Value::String(format!("val_{i}"))]);
        mutations.push(Mutation::Put {
            partition_id: PARTITION_ID,
            key,
            row,
        });
    }

    engine
        .commit(1, engine.snapshot(), mutations)
        .expect("commit initial rows");
    let snap: Snapshot = engine.snapshot();
    let target_key = format!("key_{TARGET_ID:06}").into_bytes();

    // Deterministic correctness check before timing
    let verified = engine
        .get(PARTITION_ID, &target_key, snap)
        .expect("point get succeeds");
    assert!(verified.is_some(), "target row must exist at snapshot");
    assert_eq!(
        verified.as_ref().unwrap().values()[0],
        Value::Int64(TARGET_ID)
    );

    c.bench_function("rowstore/point_get_stable_snapshot", |b| {
        b.iter(|| {
            let row = engine
                .get(
                    black_box(PARTITION_ID),
                    black_box(&target_key),
                    black_box(snap),
                )
                .unwrap();
            black_box(row);
        });
    });
}

// -----------------------------------------------------------------------------
// 2. Colstore: equality zone-map scan (100 blocks x 1024 rows)
// -----------------------------------------------------------------------------

fn bench_colstore_zone_map_scan(c: &mut Criterion) {
    const NUM_BLOCKS: usize = 100;
    const ROWS_PER_BLOCK: usize = 1024;
    const TARGET_BLOCK: usize = 73;
    const TARGET_ROW_IN_BLOCK: usize = 42;

    let dir = TempDir::new().expect("tempdir");
    let segment_path = dir.path().join("zone_map_bench.col");

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
        ColumnDef {
            name: "payload".to_string(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .expect("valid schema");

    let options = SegmentOptions::new().with_rows_per_block(ROWS_PER_BLOCK);

    let mut rows = Vec::with_capacity(NUM_BLOCKS * ROWS_PER_BLOCK);
    for b in 0..NUM_BLOCKS {
        let block_base = (b as i64) * 10_000;
        for r in 0..ROWS_PER_BLOCK {
            let id = block_base + (r as i64);
            let payload = format!("payload_b{b}_r{r}");
            rows.push(Row::new(vec![Value::Int64(id), Value::String(payload)]));
        }
    }

    let meta = SegmentWriter::write(&segment_path, &schema, rows, &options)
        .expect("write segment successfully");
    assert_eq!(meta.row_count, (NUM_BLOCKS * ROWS_PER_BLOCK) as u64);
    assert_eq!(meta.block_count, NUM_BLOCKS);

    let reader = SegmentReader::open(&segment_path).expect("open segment successfully");
    assert_eq!(reader.block_count(), NUM_BLOCKS);

    let target_id = (TARGET_BLOCK as i64) * 10_000 + (TARGET_ROW_IN_BLOCK as i64);
    let request = ScanRequest::new(
        vec![1],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Int64(target_id),
        }),
    );

    // Deterministic correctness assertion mandated by prompt:
    // Assert stats 100/99/1 and exactly one result before timing.
    let pre_result = reader.scan(&request).expect("scan succeeds");
    assert_eq!(
        pre_result.stats.candidate_blocks, 100,
        "candidate blocks must be 100"
    );
    assert_eq!(
        pre_result.stats.skipped_blocks, 99,
        "skipped blocks must be 99"
    );
    assert_eq!(
        pre_result.stats.decoded_blocks, 1,
        "decoded blocks must be 1"
    );
    assert_eq!(pre_result.batches.len(), 1, "must return exactly one batch");
    assert_eq!(
        pre_result.batches[0].num_rows(),
        1,
        "must return exactly one row result"
    );

    c.bench_function("colstore/equality_zone_map_scan", |b| {
        b.iter(|| {
            let result = reader.scan(black_box(&request)).unwrap();
            black_box(result);
        });
    });
}

// -----------------------------------------------------------------------------
// 3. Conversion: LocalConverter row->column on fresh fixed fixture per iteration
// -----------------------------------------------------------------------------

struct ConvertFixture {
    _dir: TempDir,
    converter: LocalConverter,
    part_id: PartitionId,
}

fn setup_convert_fixture() -> ConvertFixture {
    let dir = TempDir::new().expect("tempdir");
    let cat_dir = dir.path().join("catalog");
    let row_dir = dir.path().join("rowstore");
    let col_dir = dir.path().join("colstore");

    let cat_store = Arc::new(LocalCatalogStore::open(cat_dir).expect("open catalog"));
    let engine = Arc::new(Engine::open(EngineOptions::new(row_dir)).expect("open rowstore"));

    let schema = Schema::new(vec![
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
    .expect("valid schema");

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);

    let table = TableDescriptor::new(
        table_id,
        "bench_convert_table",
        schema,
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
    cat_store
        .compare_and_set(0, snap)
        .expect("initialize catalog snapshot");

    // Populate fixed rows in the rowstore
    const ROW_COUNT: i64 = 100;
    let mut mutations = Vec::with_capacity(ROW_COUNT as usize);
    for i in 0..ROW_COUNT {
        let key = encode_key(&[Value::Int64(i)]).expect("encode key");
        let row = Row::new(vec![Value::Int64(i), Value::String(format!("val_{i}"))]);
        mutations.push(Mutation::Put {
            partition_id: part_id.as_u64(),
            key,
            row,
        });
    }
    engine
        .commit(1, engine.snapshot(), mutations)
        .expect("commit rows");

    let converter = LocalConverter::new(
        cat_store,
        engine,
        col_dir,
        SegmentOptions::new().with_rows_per_block(64),
    );

    ConvertFixture {
        _dir: dir,
        converter,
        part_id,
    }
}

fn bench_convert_row_to_column(c: &mut Criterion) {
    // Deterministic correctness check before timing
    {
        let check_fixture = setup_convert_fixture();
        let manifest = check_fixture
            .converter
            .convert_partition(check_fixture.part_id)
            .expect("convert partition succeeds");
        assert_eq!(
            manifest.segment_count(),
            1,
            "manifest must contain 1 columnar segment"
        );
        assert_eq!(
            manifest.total_rows(),
            100,
            "columnar segment must contain 100 rows"
        );
    }

    c.bench_function("convert/local_converter_row_to_column", |b| {
        b.iter_batched(
            setup_convert_fixture,
            |fixture| {
                let manifest = fixture
                    .converter
                    .convert_partition(black_box(fixture.part_id))
                    .unwrap();
                black_box(manifest);
            },
            BatchSize::PerIteration,
        );
    });
}

// -----------------------------------------------------------------------------
// 4. Movement: fixed CSV import on fresh fixture
// -----------------------------------------------------------------------------

struct MovementCsvFixture {
    _dir: TempDir,
    mover: LocalDataMover,
    cat_store: Arc<LocalCatalogStore>,
    txn_manager: Arc<TransactionManager>,
    options: CopyOptions,
    csv_bytes: Vec<u8>,
}

fn setup_movement_csv_fixture() -> MovementCsvFixture {
    let dir = TempDir::new().expect("tempdir");
    let cat_dir = dir.path().join("catalog");
    let row_dir = dir.path().join("rowstore");
    let journal_path = dir.path().join("txn.journal");
    let mover_dir = dir.path().join("movement");

    let cat_store = Arc::new(LocalCatalogStore::open(cat_dir).expect("open catalog"));
    let engine = Arc::new(Engine::open(EngineOptions::new(row_dir)).expect("open rowstore"));
    let txn_manager =
        Arc::new(TransactionManager::open(journal_path).expect("open transaction manager"));

    let participant = Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    ));
    txn_manager.register_participant(participant);

    let mover = LocalDataMover::new(mover_dir).expect("init mover");

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "name".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
        ColumnDef {
            name: "score".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: false,
        },
    ])
    .expect("valid schema");

    let table = TableDescriptor::new(
        table_id,
        "bench_csv_table",
        schema,
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
    cat_store
        .compare_and_set(0, snap)
        .expect("initialize catalog");

    let mut csv_string = String::from("id,name,score\n");
    for i in 1..=50 {
        csv_string.push_str(&format!("{i},name_{i},{}\n", i * 10));
    }
    let csv_bytes = csv_string.into_bytes();

    let options = CopyOptions::new(
        "job_bench_csv",
        table_id,
        tablet_id,
        DataFormat::Csv,
        "memory.csv",
    )
    .with_has_header(true);

    MovementCsvFixture {
        _dir: dir,
        mover,
        cat_store,
        txn_manager,
        options,
        csv_bytes,
    }
}

fn bench_movement_csv_import(c: &mut Criterion) {
    // Deterministic correctness check before timing
    {
        let fixture = setup_movement_csv_fixture();
        let report = fixture
            .mover
            .copy_from_csv_reader(
                &fixture.options,
                fixture.cat_store.as_ref(),
                &fixture.txn_manager,
                fixture.csv_bytes.as_slice(),
            )
            .expect("csv import succeeds");
        assert_eq!(report.records_read, 50, "must read 50 CSV records");
        assert_eq!(report.records_committed, 50, "must commit 50 CSV records");
        assert_eq!(report.records_skipped, 0, "no CSV records should fail");
    }

    c.bench_function("movement/fixed_csv_import", |b| {
        b.iter_batched(
            setup_movement_csv_fixture,
            |fixture| {
                let report = fixture
                    .mover
                    .copy_from_csv_reader(
                        black_box(&fixture.options),
                        black_box(fixture.cat_store.as_ref()),
                        black_box(&fixture.txn_manager),
                        black_box(fixture.csv_bytes.as_slice()),
                    )
                    .unwrap();
                black_box(report);
            },
            BatchSize::PerIteration,
        );
    });
}

// -----------------------------------------------------------------------------
// 5. Coordinator: placement planning & local leadership/fenced CAS workflow
// -----------------------------------------------------------------------------

fn bench_coord_placement_planning(c: &mut Criterion) {
    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .expect("valid schema");

    const NUM_TABLETS: usize = 10;
    let mut tablets_desc = Vec::with_capacity(NUM_TABLETS);
    let mut tablet_ids = Vec::with_capacity(NUM_TABLETS);
    let mut replicas = Vec::with_capacity(NUM_TABLETS);

    for i in 0..NUM_TABLETS {
        let tid = TabletId::new(100 + i as u64);
        let pid = PartitionId::new(10);
        let rid = ReplicaId::new(1000 + i as u64);
        tablet_ids.push(tid);
        tablets_desc.push(TabletDescriptor::new(tid, pid, 0, vec![rid], 1));
        replicas.push(ReplicaDescriptor::new(
            rid,
            tid,
            NodeId::new((i % 3 + 1) as u64),
            true,
            true,
            1,
        ));
    }

    let table = TableDescriptor::new(
        TableId::new(1),
        "bench_placement_table",
        schema,
        vec![0],
        vec![PartitionId::new(10)],
        1,
    );
    let partition = PartitionDescriptor::new(
        PartitionId::new(10),
        TableId::new(1),
        "p0",
        StorageDescriptor::Row,
        tablet_ids,
        1,
    );

    let snapshot = CatalogSnapshot::new(1, vec![table], vec![partition], tablets_desc, replicas);

    let candidates = vec![
        NodeId::new(1),
        NodeId::new(2),
        NodeId::new(3),
        NodeId::new(4),
        NodeId::new(5),
        NodeId::new(6),
    ];
    const TARGET_RF: usize = 3;

    // Deterministic correctness check before timing
    let initial_plan =
        plan_placement(&snapshot, &candidates, TARGET_RF).expect("plan placement succeeds");
    assert_eq!(
        initial_plan.tablets.len(),
        NUM_TABLETS,
        "placement plan must contain all tablets"
    );
    for p in &initial_plan.tablets {
        assert_eq!(
            p.target_nodes.len(),
            TARGET_RF,
            "each tablet must reach target RF"
        );
    }

    c.bench_function("coord/placement_planning", |b| {
        b.iter(|| {
            let plan = plan_placement(
                black_box(&snapshot),
                black_box(&candidates),
                black_box(TARGET_RF),
            )
            .unwrap();
            black_box(plan);
        });
    });
}

struct CoordCasFixture {
    _dir: TempDir,
    coord: LocalCoordinator,
    cat_store: Arc<LocalCatalogStore>,
    next_snap: CatalogSnapshot,
}

fn setup_coord_cas_fixture() -> CoordCasFixture {
    let dir = TempDir::new().expect("tempdir");
    let coord_dir = dir.path().join("coord");
    let cat_dir = dir.path().join("catalog");

    let coord = LocalCoordinator::open(coord_dir).expect("open coordinator");
    let cat_store = Arc::new(LocalCatalogStore::open(cat_dir).expect("open catalog"));

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .expect("valid schema");

    let make_snap = |gen: u64| {
        let table = TableDescriptor::new(
            TableId::new(1),
            "bench_coord_table",
            schema.clone(),
            vec![0],
            vec![PartitionId::new(10)],
            gen,
        );
        let partition = PartitionDescriptor::new(
            PartitionId::new(10),
            TableId::new(1),
            "p0",
            StorageDescriptor::Row,
            vec![TabletId::new(100)],
            gen,
        );
        let tablet = TabletDescriptor::new(
            TabletId::new(100),
            PartitionId::new(10),
            0,
            vec![ReplicaId::new(1000)],
            gen,
        );
        let replica = ReplicaDescriptor::new(
            ReplicaId::new(1000),
            TabletId::new(100),
            NodeId::new(1),
            true,
            true,
            gen,
        );
        CatalogSnapshot::new(
            gen,
            vec![table],
            vec![partition],
            vec![tablet],
            vec![replica],
        )
    };

    let snap1 = make_snap(1);
    let next_snap = make_snap(2);

    cat_store
        .compare_and_set(0, snap1)
        .expect("initialize catalog at generation 1");

    CoordCasFixture {
        _dir: dir,
        coord,
        cat_store,
        next_snap,
    }
}

fn bench_coord_leadership_fenced_cas(c: &mut Criterion) {
    const SCOPE: &str = "scope_tablet_100";

    // Deterministic correctness check before timing
    {
        let fixture = setup_coord_cas_fixture();
        let leadership = fixture
            .coord
            .acquire_leadership(SCOPE, NodeId::new(1))
            .expect("acquire leadership succeeds");
        fixture
            .coord
            .fenced_catalog_compare_and_set(
                SCOPE,
                leadership.token,
                fixture.cat_store.as_ref(),
                1,
                fixture.next_snap,
            )
            .expect("fenced CAS succeeds");
        assert_eq!(
            fixture
                .cat_store
                .current_generation()
                .expect("current generation"),
            2,
            "catalog generation must advance to 2"
        );
    }

    c.bench_function("coord/leadership_fenced_cas", |b| {
        b.iter_batched(
            setup_coord_cas_fixture,
            |fixture| {
                let leadership = fixture
                    .coord
                    .acquire_leadership(black_box(SCOPE), black_box(NodeId::new(1)))
                    .unwrap();
                fixture
                    .coord
                    .fenced_catalog_compare_and_set(
                        black_box(SCOPE),
                        black_box(leadership.token),
                        black_box(fixture.cat_store.as_ref()),
                        black_box(1),
                        black_box(fixture.next_snap),
                    )
                    .unwrap();
            },
            BatchSize::PerIteration,
        );
    });
}

// -----------------------------------------------------------------------------
// Criterion Main Group
// -----------------------------------------------------------------------------

criterion_group! {
    name = benches;
    config = custom_criterion();
    targets =
        bench_rowstore_point_get,
        bench_colstore_zone_map_scan,
        bench_convert_row_to_column,
        bench_movement_csv_import,
        bench_coord_placement_planning,
        bench_coord_leadership_fenced_cas,
}
criterion_main!(benches);
