//! Integration tests for Phase 5 Tasks 3-4: Schema-aware bounded streaming CSV and JSONL import/export.

use std::fs;
use std::sync::Arc;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    CatalogSnapshot, NodeId, PartitionDescriptor, PartitionId, ReplicaDescriptor, ReplicaId,
    StorageDescriptor, TableDescriptor, TableId, TabletDescriptor, TabletId,
};
use htap_common::{encode_key, ColumnDef, DataType, HtapError, Mutation, Row, Schema, Value};
use htap_movement::{collapse_entries_to_rows, CopyOptions, DataFormat, LocalDataMover};
use htap_rowstore::{Engine, EngineOptions};
use htap_txn::{
    ParticipantId, ParticipantWork, RowstoreParticipant, TransactionManager, TransactionRequest,
};
use tempfile::TempDir;

struct TestContext {
    _temp: TempDir,
    cat_store: Arc<LocalCatalogStore>,
    engine: Arc<Engine>,
    txn_manager: Arc<TransactionManager>,
    mover: LocalDataMover,
    table_id: TableId,
    part_id: PartitionId,
    tablet_id: TabletId,
}

fn create_test_context(schema: Schema, pk_indices: Vec<usize>) -> TestContext {
    let temp = tempfile::tempdir().unwrap();
    let cat_dir = temp.path().join("catalog");
    let row_dir = temp.path().join("rowstore");
    let journal_path = temp.path().join("txn.journal");
    let mover_dir = temp.path().join("movement");

    let cat_store = Arc::new(LocalCatalogStore::open(cat_dir).unwrap());
    let engine = Arc::new(Engine::open(EngineOptions::new(row_dir)).unwrap());
    let txn_manager = Arc::new(TransactionManager::open(journal_path).unwrap());

    // Register rowstore participant with participant ID 1
    let participant = Arc::new(RowstoreParticipant::new(
        ParticipantId::new(1),
        Arc::clone(&engine),
    ));
    txn_manager.register_participant(participant);

    let mover = LocalDataMover::new(mover_dir).unwrap();

    let table_id = TableId::new(1);
    let part_id = PartitionId::new(10);
    let tablet_id = TabletId::new(100);
    let replica_id = ReplicaId::new(1000);

    let table = TableDescriptor::new(
        table_id,
        "test_table",
        schema.clone(),
        pk_indices,
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

    TestContext {
        _temp: temp,
        cat_store,
        engine,
        txn_manager,
        mover,
        table_id,
        part_id,
        tablet_id,
    }
}

fn all_types_schema() -> Schema {
    Schema::new(vec![
        ColumnDef {
            name: "c_id".into(),
            data_type: DataType::Int32,
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
            name: "c_i64".into(),
            data_type: DataType::Int64,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_f64".into(),
            data_type: DataType::Float64,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "c_str".into(),
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
            name: "c_ts".into(),
            data_type: DataType::Timestamp,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap()
}

#[test]
fn test_all_types_and_nulls_csv() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let csv_file = ctx._temp.path().join("all_types.csv");
    let export_file = ctx._temp.path().join("all_types_export.csv");

    let csv_data = "c_id,c_bool,c_i64,c_f64,c_str,c_bytes,c_ts\n\
                    1,true,100,2.5,hello,010203,1700000000\n\
                    2,\\N,\\N,\\N,\\N,\\N,\\N\n";
    fs::write(&csv_file, csv_data).unwrap();

    let options = CopyOptions::new(
        "import_all_types_csv",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    );

    let report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_read, 2);
    assert_eq!(report.records_committed, 2);
    assert_eq!(report.rows_written, 2);
    assert_eq!(report.records_skipped, 0);

    // Verify rows in engine
    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 2);

    assert_eq!(
        rows[0].values(),
        &[
            Value::Int32(1),
            Value::Bool(true),
            Value::Int64(100),
            Value::Float64(2.5),
            Value::String("hello".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Timestamp(1700000000),
        ]
    );

    assert_eq!(
        rows[1].values(),
        &[
            Value::Int32(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    );

    // Export to CSV and verify
    let export_options = CopyOptions::new(
        "export_all_types_csv",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &export_file,
    );
    let exp_report = ctx
        .mover
        .copy_to_csv(&export_options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    assert_eq!(exp_report.records_read, 2);
    assert_eq!(exp_report.records_committed, 2);

    let exported_content = fs::read_to_string(&export_file).unwrap();
    assert_eq!(exported_content, csv_data);
}

#[test]
fn test_all_types_and_nulls_jsonl() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let jsonl_file = ctx._temp.path().join("all_types.jsonl");
    let export_file = ctx._temp.path().join("all_types_export.jsonl");

    let jsonl_data = "{\"c_id\":1,\"c_bool\":true,\"c_i64\":100,\"c_f64\":2.5,\"c_str\":\"hello\",\"c_bytes\":\"010203\",\"c_ts\":1700000000}\n\
                      {\"c_id\":2,\"c_bool\":null,\"c_i64\":null,\"c_f64\":null,\"c_str\":null,\"c_bytes\":null,\"c_ts\":null}\n";
    fs::write(&jsonl_file, jsonl_data).unwrap();

    let options = CopyOptions::new(
        "import_all_types_jsonl",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::JsonLines,
        &jsonl_file,
    );

    let report = ctx
        .mover
        .copy_from_jsonl(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_read, 2);
    assert_eq!(report.records_committed, 2);

    // Export to JSONL and verify
    let export_options = CopyOptions::new(
        "export_all_types_jsonl",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::JsonLines,
        &export_file,
    );
    let exp_report = ctx
        .mover
        .copy_to_jsonl(&export_options, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    assert_eq!(exp_report.records_read, 2);
    let exported_content = fs::read_to_string(&export_file).unwrap();
    assert_eq!(exported_content, jsonl_data);
}

#[test]
fn test_reordered_csv_headers() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let csv_file = ctx._temp.path().join("reordered.csv");

    // Header has columns in completely different order than schema
    let csv_data = "c_str,c_id,c_bytes,c_bool,c_ts,c_f64,c_i64\n\
                    world,10,aabbcc,false,1600000000,99.5,42\n";
    fs::write(&csv_file, csv_data).unwrap();

    let options = CopyOptions::new(
        "import_reordered",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    );

    let report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_read, 1);
    assert_eq!(report.records_committed, 1);

    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 1);

    // Position matches schema declaration order: c_id, c_bool, c_i64, c_f64, c_str, c_bytes, c_ts
    assert_eq!(
        rows[0].values(),
        &[
            Value::Int32(10),
            Value::Bool(false),
            Value::Int64(42),
            Value::Float64(99.5),
            Value::String("world".into()),
            Value::Bytes(vec![0xaa, 0xbb, 0xcc]),
            Value::Timestamp(1600000000),
        ]
    );
}

#[test]
fn test_headerless_csv_import() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let csv_file = ctx._temp.path().join("headerless.csv");

    // Two rows in schema declaration order: c_id, c_bool, c_i64, c_f64, c_str, c_bytes, c_ts
    // Crucially: NO header row. Row 1 MUST NOT be consumed as a header row!
    let csv_data = "1,true,100,1.5,first,deadbeef,1600000001\n\
                    2,false,200,2.5,second,cafebabe,1600000002\n";
    fs::write(&csv_file, csv_data).unwrap();

    let options = CopyOptions::new(
        "import_headerless",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    )
    .with_has_header(false);

    let report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_read, 2);
    assert_eq!(report.records_committed, 2);
    assert_eq!(report.records_skipped, 0);

    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 2);

    // Row 1 must be present and match first row
    assert_eq!(
        rows[0].values(),
        &[
            Value::Int32(1),
            Value::Bool(true),
            Value::Int64(100),
            Value::Float64(1.5),
            Value::String("first".into()),
            Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            Value::Timestamp(1600000001),
        ]
    );
    // Row 2 must be present and match second row
    assert_eq!(
        rows[1].values(),
        &[
            Value::Int32(2),
            Value::Bool(false),
            Value::Int64(200),
            Value::Float64(2.5),
            Value::String("second".into()),
            Value::Bytes(vec![0xca, 0xfe, 0xba, 0xbe]),
            Value::Timestamp(1600000002),
        ]
    );
}

#[test]
fn test_headerless_csv_reader_import() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let csv_data = "10,true,1000,10.5,reader_row1,112233,1600000010\n\
                    20,false,2000,20.5,reader_row2,445566,1600000020\n";

    let options = CopyOptions::new(
        "import_headerless_reader",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        "/unused/stream/path",
    )
    .with_has_header(false);

    let report = ctx
        .mover
        .copy_from_csv_reader(
            &options,
            ctx.cat_store.as_ref(),
            &ctx.txn_manager,
            csv_data.as_bytes(),
        )
        .unwrap();

    assert_eq!(report.records_read, 2);
    assert_eq!(report.records_committed, 2);

    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values()[0], Value::Int32(10));
    assert_eq!(rows[1].values()[0], Value::Int32(20));
}

#[test]
fn test_headerless_csv_field_count_mismatch() {
    let ctx = create_test_context(all_types_schema(), vec![0]);

    // 1. Too few fields (schema has 7 columns, row has 6)
    {
        let file = ctx._temp.path().join("few_fields.csv");
        fs::write(&file, "1,true,100,1.5,first,deadbeef\n").unwrap();
        let opts = CopyOptions::new(
            "few_fields",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        )
        .with_has_header(false);
        let err = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(err
            .to_string()
            .contains("field count (6) does not match schema width (7)"));
    }

    // 2. Too many fields (schema has 7 columns, row has 8)
    {
        let file = ctx._temp.path().join("many_fields.csv");
        fs::write(&file, "1,true,100,1.5,first,deadbeef,1600000001,extra\n").unwrap();
        let opts = CopyOptions::new(
            "many_fields",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        )
        .with_has_header(false);
        let err = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(err
            .to_string()
            .contains("field count (8) does not match schema width (7)"));
    }

    // 3. max_errors allows skipping row with wrong field count
    {
        let file = ctx._temp.path().join("max_errors_field_count.csv");
        let data = "1,true,100,1.5,first,deadbeef,1600000001\n\
                    bad_row_too_short\n\
                    2,false,200,2.5,second,cafebabe,1600000002\n";
        fs::write(&file, data).unwrap();
        let opts = CopyOptions::new(
            "max_errors_field_count",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        )
        .with_has_header(false)
        .with_max_errors(1);
        let report = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap();
        assert_eq!(report.records_read, 3);
        assert_eq!(report.records_committed, 2);
        assert_eq!(report.records_skipped, 1);
    }
}

#[test]
fn test_headerless_csv_export_import_roundtrip() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let export_file = ctx._temp.path().join("roundtrip_no_header.csv");

    // Insert 2 rows first
    let initial_csv = "1,true,10,1.0,one,01,1000\n\
                       2,false,20,2.0,two,02,2000\n";
    let init_file = ctx._temp.path().join("initial.csv");
    fs::write(&init_file, initial_csv).unwrap();
    let init_opts = CopyOptions::new(
        "roundtrip_init",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &init_file,
    )
    .with_has_header(false);
    ctx.mover
        .copy_from_csv(&init_opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    // Export without header
    let exp_opts = CopyOptions::new(
        "roundtrip_export",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &export_file,
    )
    .with_has_header(false);
    let exp_report = ctx
        .mover
        .copy_to_csv(&exp_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();
    assert_eq!(exp_report.records_read, 2);

    let content = fs::read_to_string(&export_file).unwrap();
    // Verify no header line (neither "c_id" nor schema names appear as first line)
    assert!(!content.starts_with("c_id"));
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 2);

    // Import into a new table context
    let ctx2 = create_test_context(all_types_schema(), vec![0]);
    let imp_opts = CopyOptions::new(
        "roundtrip_import",
        ctx2.table_id,
        ctx2.tablet_id,
        DataFormat::Csv,
        &export_file,
    )
    .with_has_header(false);
    let imp_report = ctx2
        .mover
        .copy_from_csv(&imp_opts, ctx2.cat_store.as_ref(), &ctx2.txn_manager)
        .unwrap();
    assert_eq!(imp_report.records_read, 2);
    assert_eq!(imp_report.records_committed, 2);

    let entries = ctx2
        .engine
        .scan_partition(ctx2.part_id.as_u64(), ctx2.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values()[0], Value::Int32(1));
    assert_eq!(rows[1].values()[0], Value::Int32(2));
}

#[test]
fn test_headerless_csv_resume() {
    let ctx = create_test_context(all_types_schema(), vec![0]);
    let csv_file = ctx._temp.path().join("headerless_resume.csv");

    let csv_data = "1,true,10,1.0,one,01,1000\n\
                    2,false,20,2.0,two,02,2000\n\
                    3,true,30,3.0,three,03,3000\n";
    fs::write(&csv_file, csv_data).unwrap();

    let options = CopyOptions::new(
        "headerless_resume_job",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    )
    .with_has_header(false)
    .with_batch_rows(1); // batch size 1 forces checkpoints after each row

    let report1 = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();
    assert_eq!(report1.records_read, 3);
    assert_eq!(report1.records_committed, 3);

    // Terminal idempotent retry
    let report2 = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();
    assert_eq!(report2.records_read, 3);
    assert_eq!(report2.records_committed, 3);

    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 3);
}

#[test]
fn test_malformed_inputs_csv() {
    let ctx = create_test_context(all_types_schema(), vec![0]);

    // 1. Duplicate column in header
    {
        let file = ctx._temp.path().join("bad_hdr1.csv");
        fs::write(
            &file,
            "c_id,c_bool,c_i64,c_f64,c_str,c_bytes,c_id\n1,true,1,1.0,s,01,1\n",
        )
        .unwrap();
        let opts = CopyOptions::new(
            "bad_hdr1",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 2. Unknown column in header
    {
        let file = ctx._temp.path().join("bad_hdr2.csv");
        fs::write(
            &file,
            "c_id,c_bool,c_i64,c_f64,c_str,c_bytes,unknown\n1,true,1,1.0,s,01,1\n",
        )
        .unwrap();
        let opts = CopyOptions::new(
            "bad_hdr2",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 3. Missing column in header
    {
        let file = ctx._temp.path().join("bad_hdr3.csv");
        fs::write(
            &file,
            "c_id,c_bool,c_i64,c_f64,c_str,c_bytes\n1,true,1,1.0,s,01\n",
        )
        .unwrap();
        let opts = CopyOptions::new(
            "bad_hdr3",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 4. Non-nullable PK column given NULL (\N) with max_errors = 0 -> fails immediately
    {
        let file = ctx._temp.path().join("bad_pk_null.csv");
        fs::write(
            &file,
            "c_id,c_bool,c_i64,c_f64,c_str,c_bytes,c_ts\n\\N,true,1,1.0,s,01,1\n",
        )
        .unwrap();
        let opts = CopyOptions::new(
            "bad_pk_null",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 5. Type mismatch (invalid int) with max_errors = 1 (skips error and commits good rows)
    {
        let file = ctx._temp.path().join("max_errors_csv.csv");
        let data = "c_id,c_bool,c_i64,c_f64,c_str,c_bytes,c_ts\n\
                    10,true,1,1.0,s,01,1\n\
                    invalid_id,true,1,1.0,s,01,1\n\
                    20,false,2,2.0,t,02,2\n";
        fs::write(&file, data).unwrap();
        let opts = CopyOptions::new(
            "max_errors_csv",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::Csv,
            &file,
        )
        .with_max_errors(1);
        let report = ctx
            .mover
            .copy_from_csv(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap();
        assert_eq!(report.records_read, 3);
        assert_eq!(report.records_committed, 2);
        assert_eq!(report.records_skipped, 1);
    }
}

#[test]
fn test_malformed_inputs_jsonl() {
    let ctx = create_test_context(all_types_schema(), vec![0]);

    // 1. Missing field in JSON object
    {
        let file = ctx._temp.path().join("missing_field.jsonl");
        fs::write(&file, "{\"c_id\":1,\"c_bool\":true}\n").unwrap();
        let opts = CopyOptions::new(
            "missing_field",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::JsonLines,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_jsonl(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 2. Extra unknown field in JSON object
    {
        let file = ctx._temp.path().join("extra_field.jsonl");
        let data = "{\"c_id\":1,\"c_bool\":true,\"c_i64\":100,\"c_f64\":2.5,\"c_str\":\"hello\",\"c_bytes\":\"010203\",\"c_ts\":1700000000,\"extra\":42}\n";
        fs::write(&file, data).unwrap();
        let opts = CopyOptions::new(
            "extra_field",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::JsonLines,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_jsonl(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 3. Non-nullable PK column given null
    {
        let file = ctx._temp.path().join("json_null_pk.jsonl");
        let data = "{\"c_id\":null,\"c_bool\":true,\"c_i64\":100,\"c_f64\":2.5,\"c_str\":\"hello\",\"c_bytes\":\"010203\",\"c_ts\":1700000000}\n";
        fs::write(&file, data).unwrap();
        let opts = CopyOptions::new(
            "json_null_pk",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::JsonLines,
            &file,
        );
        let err = ctx
            .mover
            .copy_from_jsonl(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    // 4. max_errors skip behavior in JSONL
    {
        let file = ctx._temp.path().join("max_errors_jsonl.jsonl");
        let data = "{\"c_id\":1,\"c_bool\":true,\"c_i64\":100,\"c_f64\":2.5,\"c_str\":\"hello\",\"c_bytes\":\"010203\",\"c_ts\":1700000000}\n\
                    {\"malformed_json: }\n\
                    {\"c_id\":2,\"c_bool\":false,\"c_i64\":200,\"c_f64\":3.5,\"c_str\":\"world\",\"c_bytes\":\"040506\",\"c_ts\":1800000000}\n";
        fs::write(&file, data).unwrap();
        let opts = CopyOptions::new(
            "max_errors_jsonl",
            ctx.table_id,
            ctx.tablet_id,
            DataFormat::JsonLines,
            &file,
        )
        .with_max_errors(1);
        let report = ctx
            .mover
            .copy_from_jsonl(&opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
            .unwrap();
        assert_eq!(report.records_read, 3);
        assert_eq!(report.records_committed, 2);
        assert_eq!(report.records_skipped, 1);
    }
}

#[test]
fn test_composite_pk() {
    let schema = Schema::new(vec![
        ColumnDef {
            name: "tenant_id".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "user_id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "display_name".into(),
            data_type: DataType::String,
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();

    let ctx = create_test_context(schema, vec![0, 1]);
    let csv_file = ctx._temp.path().join("composite.csv");
    let csv_data = "tenant_id,user_id,display_name\n\
                    1,1001,Alice\n\
                    1,1002,Bob\n\
                    2,1001,Charlie\n";
    fs::write(&csv_file, csv_data).unwrap();

    let options = CopyOptions::new(
        "import_composite",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    );

    let report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_committed, 3);

    // Verify key encoding works with composite PK
    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get(2), Some(&Value::String("Alice".into())));
    assert_eq!(rows[1].get(2), Some(&Value::String("Bob".into())));
    assert_eq!(rows[2].get(2), Some(&Value::String("Charlie".into())));

    // Upsert Bob with Alice2 name
    let csv_update = "tenant_id,user_id,display_name\n1,1002,BobUpdated\n";
    let update_file = ctx._temp.path().join("composite_up.csv");
    fs::write(&update_file, csv_update).unwrap();

    let update_opts = CopyOptions::new(
        "import_composite_up",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &update_file,
    );
    ctx.mover
        .copy_from_csv(&update_opts, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    let entries_after = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows_after = collapse_entries_to_rows(&entries_after);
    assert_eq!(rows_after.len(), 3);
    assert_eq!(
        rows_after[1].get(2),
        Some(&Value::String("BobUpdated".into()))
    );
}

#[test]
fn test_batch_boundaries() {
    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);
    let csv_file = ctx._temp.path().join("batches.csv");

    // 25 records with batch_rows = 10 -> 3 batches (10, 10, 5)
    let mut data = String::from("id,val\n");
    for i in 1..=25 {
        data.push_str(&format!("{i},{i}\n"));
    }
    fs::write(&csv_file, data).unwrap();

    let options = CopyOptions::new(
        "import_batches",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    )
    .with_batch_rows(10);

    let report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_read, 25);
    assert_eq!(report.records_committed, 25);
    assert_eq!(report.rows_written, 25);

    let job = ctx.mover.load_job("import_batches").unwrap().unwrap();
    assert_eq!(job.counters.records_read, 25);
    assert_eq!(job.counters.records_committed, 25);

    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 25);
}

#[test]
fn test_restart_resume_retry_convergence() {
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
            nullable: true,
            primary_key: false,
        },
    ])
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);
    let csv_file = ctx._temp.path().join("resume.csv");

    let mut data = String::from("id,name\n");
    for i in 1..=30 {
        data.push_str(&format!("{i},name_{i}\n"));
    }
    fs::write(&csv_file, &data).unwrap();

    let options = CopyOptions::new(
        "resume_job_1",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    )
    .with_batch_rows(10);

    // 1. Manually simulate an interrupted run by committing batch 1 and checkpointing
    let job = ctx
        .mover
        .start_copy(htap_movement::MovementJobKind::Import, &options)
        .unwrap();
    assert!(job.is_running());

    // Commit first 10 rows into rowstore
    let mut mutations = Vec::new();
    for i in 1..=10 {
        let pk = encode_key(&[Value::Int32(i)]).unwrap();
        mutations.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: pk,
            row: Row::new(vec![Value::Int32(i), Value::String(format!("name_{i}"))]),
        });
    }
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
    let req = TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
        .unwrap();
    ctx.txn_manager.commit_request(req).unwrap();

    let mut counters = htap_movement::JobCounters::new();
    counters.records_read = 10;
    counters.records_committed = 10;
    counters.rows_written = 10;
    ctx.mover
        .commit_progress("resume_job_1", counters, Some("10".into()))
        .unwrap();

    // 2. Now call copy_from_csv to resume
    let report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();

    assert_eq!(report.records_read, 30);
    assert_eq!(report.records_committed, 30);

    // Verify all 30 rows in rowstore
    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 30);

    // 3. Retry completed job: terminal retry is a no-op returning cached report
    let retry_report = ctx
        .mover
        .copy_from_csv(&options, ctx.cat_store.as_ref(), &ctx.txn_manager)
        .unwrap();
    assert_eq!(retry_report.records_read, 30);
}

#[test]
fn test_non_seekable_resume_unsupported() {
    let schema = Schema::new(vec![ColumnDef {
        name: "id".into(),
        data_type: DataType::Int32,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);
    let file = ctx._temp.path().join("dummy.csv");
    fs::write(&file, "id\n1\n").unwrap();

    let options = CopyOptions::new(
        "non_seekable_test",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &file,
    );

    // Start job and checkpoint progress
    let _ = ctx
        .mover
        .start_copy(htap_movement::MovementJobKind::Import, &options)
        .unwrap();
    let mut counters = htap_movement::JobCounters::new();
    counters.records_read = 5;
    counters.records_committed = 5;
    ctx.mover
        .commit_progress("non_seekable_test", counters, Some("5".into()))
        .unwrap();

    // Attempting resume via copy_from_csv_reader should return Unsupported
    let dummy_stream = std::io::Cursor::new("id\n1\n2\n");
    let err = ctx
        .mover
        .copy_from_csv_reader(
            &options,
            ctx.cat_store.as_ref(),
            &ctx.txn_manager,
            dummy_stream,
        )
        .unwrap_err();

    assert!(matches!(err, HtapError::Unsupported(_)));
}

#[test]
fn test_commit_before_checkpoint_replay_caveat() {
    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "counter".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);

    // Demonstrate replay window: batch committed to txn_manager, but checkpoint NOT updated.
    let pk1 = encode_key(&[Value::Int32(1)]).unwrap();
    let m1 = Mutation::Put {
        partition_id: ctx.part_id.as_u64(),
        key: pk1.clone(),
        row: Row::new(vec![Value::Int32(1), Value::Int64(100)]),
    };
    let payload = RowstoreParticipant::encode_payload(std::slice::from_ref(&m1)).unwrap();
    let req1 = TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
        .unwrap();
    ctx.txn_manager.commit_request(req1).unwrap();

    // Replay the exact same mutation (as would happen if crash occurred before progress commit)
    let payload_replay = RowstoreParticipant::encode_payload(&[m1]).unwrap();
    let req2 = TransactionRequest::new(vec![ParticipantWork::new(
        ParticipantId::new(1),
        payload_replay,
    )])
    .unwrap();
    ctx.txn_manager.commit_request(req2).unwrap();

    // Verify rowstore collapsed state has exactly 1 row with value 100
    let entries = ctx
        .engine
        .scan_partition(ctx.part_id.as_u64(), ctx.engine.snapshot())
        .unwrap();
    let rows = collapse_entries_to_rows(&entries);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(1), Some(&Value::Int64(100)));
}

#[test]
fn test_csv_jsonl_roundtrip() {
    let ctx = create_test_context(all_types_schema(), vec![0]);

    // Pre-populate engine with 3 rows
    let mut mutations = Vec::new();
    for i in 1..=3 {
        let pk = encode_key(&[Value::Int32(i)]).unwrap();
        mutations.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: pk,
            row: Row::new(vec![
                Value::Int32(i),
                Value::Bool(i % 2 == 0),
                Value::Int64(i as i64 * 100),
                Value::Float64(i as f64 + 0.5),
                Value::String(format!("str_{i}")),
                Value::Bytes(vec![i as u8, (i + 1) as u8]),
                Value::Timestamp(1700000000 + i as i64),
            ]),
        });
    }
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
    let req = TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
        .unwrap();
    ctx.txn_manager.commit_request(req).unwrap();

    // Export to CSV
    let csv_file = ctx._temp.path().join("roundtrip.csv");
    let exp_csv_opts = CopyOptions::new(
        "exp_csv",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    );
    ctx.mover
        .copy_to_csv(&exp_csv_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    // Export to JSONL
    let jsonl_file = ctx._temp.path().join("roundtrip.jsonl");
    let exp_json_opts = CopyOptions::new(
        "exp_jsonl",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::JsonLines,
        &jsonl_file,
    );
    ctx.mover
        .copy_to_jsonl(&exp_json_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    // Now import CSV into a fresh context
    let ctx2 = create_test_context(all_types_schema(), vec![0]);
    let imp_csv_opts = CopyOptions::new(
        "imp_csv",
        ctx2.table_id,
        ctx2.tablet_id,
        DataFormat::Csv,
        &csv_file,
    );
    ctx2.mover
        .copy_from_csv(&imp_csv_opts, ctx2.cat_store.as_ref(), &ctx2.txn_manager)
        .unwrap();

    let entries2 = ctx2
        .engine
        .scan_partition(ctx2.part_id.as_u64(), ctx2.engine.snapshot())
        .unwrap();
    let rows2 = collapse_entries_to_rows(&entries2);
    assert_eq!(rows2.len(), 3);
    assert_eq!(rows2[0].get(4), Some(&Value::String("str_1".into())));

    // Now import JSONL into another fresh context
    let ctx3 = create_test_context(all_types_schema(), vec![0]);
    let imp_json_opts = CopyOptions::new(
        "imp_jsonl",
        ctx3.table_id,
        ctx3.tablet_id,
        DataFormat::JsonLines,
        &jsonl_file,
    );
    ctx3.mover
        .copy_from_jsonl(&imp_json_opts, ctx3.cat_store.as_ref(), &ctx3.txn_manager)
        .unwrap();

    let entries3 = ctx3
        .engine
        .scan_partition(ctx3.part_id.as_u64(), ctx3.engine.snapshot())
        .unwrap();
    let rows3 = collapse_entries_to_rows(&entries3);
    assert_eq!(rows3, rows2);
}

#[test]
fn test_deterministic_order() {
    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int32,
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
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);

    // Insert keys out of order: 50, 10, 40, 20, 30
    let ids = [50, 10, 40, 20, 30];
    let mut mutations = Vec::new();
    for &id in &ids {
        let pk = encode_key(&[Value::Int32(id)]).unwrap();
        mutations.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: pk,
            row: Row::new(vec![Value::Int32(id), Value::String(format!("val_{id}"))]),
        });
    }
    let payload = RowstoreParticipant::encode_payload(&mutations).unwrap();
    let req = TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
        .unwrap();
    ctx.txn_manager.commit_request(req).unwrap();

    // Export to CSV
    let csv_file = ctx._temp.path().join("ordered.csv");
    let exp_opts = CopyOptions::new(
        "exp_order_csv",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &csv_file,
    );
    ctx.mover
        .copy_to_csv(&exp_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();

    let content = fs::read_to_string(&csv_file).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines[0], "id,val");
    assert_eq!(lines[1], "10,val_10");
    assert_eq!(lines[2], "20,val_20");
    assert_eq!(lines[3], "30,val_30");
    assert_eq!(lines[4], "40,val_40");
    assert_eq!(lines[5], "50,val_50");
}

#[test]
fn test_pinned_export_excludes_later_writes() {
    let schema = Schema::new(vec![ColumnDef {
        name: "id".into(),
        data_type: DataType::Int32,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);

    // Write initial rows 1..=5
    let mut m1 = Vec::new();
    for i in 1..=5 {
        let pk = encode_key(&[Value::Int32(i)]).unwrap();
        m1.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: pk,
            row: Row::new(vec![Value::Int32(i)]),
        });
    }
    let p1 = RowstoreParticipant::encode_payload(&m1).unwrap();
    ctx.txn_manager
        .commit_request(
            TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), p1)]).unwrap(),
        )
        .unwrap();

    // Pin snapshot version
    let snapshot_version = ctx.engine.snapshot().version;

    // Write later rows 6..=10
    let mut m2 = Vec::new();
    for i in 6..=10 {
        let pk = encode_key(&[Value::Int32(i)]).unwrap();
        m2.push(Mutation::Put {
            partition_id: ctx.part_id.as_u64(),
            key: pk,
            row: Row::new(vec![Value::Int32(i)]),
        });
    }
    let p2 = RowstoreParticipant::encode_payload(&m2).unwrap();
    ctx.txn_manager
        .commit_request(
            TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), p2)]).unwrap(),
        )
        .unwrap();

    // Export with pinned snapshot version
    let export_file = ctx._temp.path().join("pinned_export.csv");
    let exp_opts = CopyOptions::new(
        "pinned_exp",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &export_file,
    )
    .with_pinned_version(snapshot_version);

    let report = ctx
        .mover
        .copy_to_csv(&exp_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();
    assert_eq!(report.records_read, 5);
    assert_eq!(report.records_committed, 5);

    let content = fs::read_to_string(&export_file).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 6); // header + 5 rows
    assert_eq!(lines[1], "1");
    assert_eq!(lines[5], "5");
}

#[test]
fn test_output_atomicity_and_terminal_idempotence() {
    let schema = Schema::new(vec![ColumnDef {
        name: "id".into(),
        data_type: DataType::Int32,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    let ctx = create_test_context(schema, vec![0]);
    let pk = encode_key(&[Value::Int32(1)]).unwrap();
    let m = Mutation::Put {
        partition_id: ctx.part_id.as_u64(),
        key: pk,
        row: Row::new(vec![Value::Int32(1)]),
    };
    let payload = RowstoreParticipant::encode_payload(&[m]).unwrap();
    ctx.txn_manager
        .commit_request(
            TransactionRequest::new(vec![ParticipantWork::new(ParticipantId::new(1), payload)])
                .unwrap(),
        )
        .unwrap();

    let export_file = ctx._temp.path().join("atomic_out.csv");
    let exp_opts = CopyOptions::new(
        "atomic_job",
        ctx.table_id,
        ctx.tablet_id,
        DataFormat::Csv,
        &export_file,
    );

    // Initial run
    let report1 = ctx
        .mover
        .copy_to_csv(&exp_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();
    assert_eq!(report1.records_committed, 1);
    assert!(export_file.exists());

    // Check no temp files left in directory
    let parent = export_file.parent().unwrap();
    for entry in fs::read_dir(parent).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(!name.ends_with(".tmp"));
    }

    // Terminal idempotence: rerun with same options
    let report2 = ctx
        .mover
        .copy_to_csv(&exp_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap();
    assert_eq!(report2.job_id, report1.job_id);
    assert_eq!(report2.records_committed, 1);

    // Mismatched request descriptor with same job ID must fail with Conflict
    let conflicting_opts = CopyOptions::new(
        "atomic_job",
        TableId::new(999), // Different table ID!
        ctx.tablet_id,
        DataFormat::Csv,
        &export_file,
    );
    let err = ctx
        .mover
        .copy_to_csv(&conflicting_opts, ctx.cat_store.as_ref(), &ctx.engine)
        .unwrap_err();
    assert!(matches!(err, HtapError::Conflict(_)));
}
