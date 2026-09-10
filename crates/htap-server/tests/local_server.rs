//! Comprehensive integration tests for `LocalServer`.

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    ConversionDescriptor, ConversionPhase, NodeId, ReplicaDescriptor, ReplicaId, StorageDescriptor,
    StorageFormat, TableId, TabletId,
};
use htap_common::types::{DataType, Value};
use htap_common::version::Version;
use htap_common::HtapError;
use htap_movement::{CopyOptions, DataFormat, MovementJobPhase, TabletCloneOptions};
use htap_server::LocalServer;
use htap_sql::result::{CommandResult, StatementResult};
use tempfile::TempDir;

#[test]
fn test_create_and_duplicate_rejection() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let res = server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT);")
        .unwrap();
    match res {
        StatementResult::Command(CommandResult::Ddl { affected }) => {
            assert_eq!(affected, 1);
        }
        other => panic!("expected DDL command result, got {other:?}"),
    }

    // Duplicate CREATE TABLE must fail with Conflict
    let err = server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT);")
        .unwrap_err();
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict error, got {err:?}"
    );
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn test_catalog_topology_and_reopen() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE items (id BIGINT PRIMARY KEY, title VARCHAR);")
            .unwrap();
    }

    // Directly inspect catalog snapshot on disk
    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snapshot = catalog_store.load().unwrap().expect("snapshot must exist");
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.tables.len(), 1);
    assert_eq!(snapshot.partitions.len(), 1);
    assert_eq!(snapshot.tablets.len(), 1);
    assert_eq!(snapshot.replicas.len(), 1);

    let table = &snapshot.tables[0];
    assert_eq!(table.name, "items");
    assert_eq!(table.id.as_u64(), 1);
    assert_eq!(table.primary_key, vec![0]);
    assert_eq!(table.partitions.len(), 1);
    assert_eq!(table.partitions[0].as_u64(), 1);

    let partition = &snapshot.partitions[0];
    assert_eq!(partition.id.as_u64(), 1);
    assert_eq!(partition.table_id.as_u64(), 1);
    assert_eq!(partition.name, "p0");
    assert_eq!(partition.storage, StorageDescriptor::Row);
    assert_eq!(partition.tablets.len(), 1);
    assert_eq!(partition.tablets[0].as_u64(), 1);

    let tablet = &snapshot.tablets[0];
    assert_eq!(tablet.id.as_u64(), 1);
    assert_eq!(tablet.partition_id.as_u64(), 1);
    assert_eq!(tablet.bucket, 0);
    assert_eq!(tablet.replicas.len(), 1);
    assert_eq!(tablet.replicas[0].as_u64(), 1);

    let replica = &snapshot.replicas[0];
    assert_eq!(replica.id.as_u64(), 1);
    assert_eq!(replica.tablet_id.as_u64(), 1);
    assert_eq!(replica.node_id.as_u64(), 1);
    assert!(replica.is_leader);
    assert!(replica.healthy);

    // Reopen and create second table; verify IDs advance
    let server2 = LocalServer::open(dir.path()).unwrap();
    server2
        .execute("CREATE TABLE orders (order_id BIGINT PRIMARY KEY, amount DOUBLE);")
        .unwrap();

    let snapshot2 = catalog_store
        .load()
        .unwrap()
        .expect("snapshot 2 must exist");
    assert_eq!(snapshot2.generation, 2);
    assert_eq!(snapshot2.tables.len(), 2);
    assert_eq!(snapshot2.partitions.len(), 2);
    assert_eq!(snapshot2.tablets.len(), 2);
    assert_eq!(snapshot2.replicas.len(), 2);

    let table2 = snapshot2.table_by_name("orders").unwrap();
    assert_eq!(table2.id.as_u64(), 2);
    assert_eq!(table2.partitions[0].as_u64(), 2);
}

#[test]
fn test_reordered_insert_and_select_projection() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE employees (\
            id BIGINT PRIMARY KEY, \
            name VARCHAR, \
            department VARCHAR, \
            salary INT);",
        )
        .unwrap();

    // Reordered column INSERT
    let insert_res = server
        .execute(
            "INSERT INTO employees (department, name, salary, id) \
            VALUES ('Engineering', 'Alice', 120000, 42);",
        )
        .unwrap();
    assert_eq!(insert_res, StatementResult::dml(1, Some(Version::new(2))));

    // Projection subset in custom order
    let select_res = server
        .execute("SELECT salary, name FROM employees WHERE id = 42;")
        .unwrap();
    match select_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let cols = qr.columns();
            assert_eq!(cols.len(), 2);
            assert_eq!(cols[0].name, "salary");
            assert_eq!(cols[0].data_type, DataType::Int32);
            assert_eq!(cols[1].name, "name");
            assert_eq!(cols[1].data_type, DataType::String);

            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int32(120000)));
            assert_eq!(row.get(1), Some(&Value::String("Alice".to_string())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Projection with SELECT *
    let select_all = server
        .execute("SELECT * FROM employees WHERE id = 42;")
        .unwrap();
    match select_all {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let cols = qr.columns();
            assert_eq!(cols.len(), 4);
            assert_eq!(cols[0].name, "id");
            assert_eq!(cols[1].name, "name");
            assert_eq!(cols[2].name, "department");
            assert_eq!(cols[3].name, "salary");

            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(42)));
            assert_eq!(row.get(1), Some(&Value::String("Alice".to_string())));
            assert_eq!(row.get(2), Some(&Value::String("Engineering".to_string())));
            assert_eq!(row.get(3), Some(&Value::Int32(120000)));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_composite_primary_key() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute(
            "CREATE TABLE accounts (\
            tenant_id BIGINT, \
            account_id BIGINT, \
            balance DOUBLE, \
            PRIMARY KEY (tenant_id, account_id));",
        )
        .unwrap();

    // Insert multiple composite rows
    server
        .execute("INSERT INTO accounts (tenant_id, account_id, balance) VALUES (1, 100, 50.5);")
        .unwrap();
    server
        .execute("INSERT INTO accounts (tenant_id, account_id, balance) VALUES (1, 200, 75.0);")
        .unwrap();
    server
        .execute("INSERT INTO accounts (tenant_id, account_id, balance) VALUES (2, 100, 100.0);")
        .unwrap();

    // Query composite row
    let res = server
        .execute("SELECT balance FROM accounts WHERE tenant_id = 1 AND account_id = 200;")
        .unwrap();
    match res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(75.0)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Delete composite row
    let del_res = server
        .execute("DELETE FROM accounts WHERE tenant_id = 1 AND account_id = 200;")
        .unwrap();
    assert_eq!(del_res, StatementResult::dml(1, Some(Version::new(5))));

    // Verify deleted
    let absent = server
        .execute("SELECT balance FROM accounts WHERE tenant_id = 1 AND account_id = 200;")
        .unwrap();
    match absent {
        StatementResult::Query(qr) => {
            assert!(qr.is_empty());
            assert_eq!(qr.columns().len(), 1);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Verify other composite rows remain
    let remaining = server
        .execute("SELECT balance FROM accounts WHERE tenant_id = 1 AND account_id = 100;")
        .unwrap();
    match remaining {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(50.5)));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_absent_select_metadata_no_rows() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE products (sku BIGINT PRIMARY KEY, name VARCHAR, price INT);")
        .unwrap();

    let res = server
        .execute("SELECT price, name FROM products WHERE sku = 9999;")
        .unwrap();
    match res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
            assert!(qr.is_empty());
            assert_eq!(qr.columns().len(), 2);
            assert_eq!(qr.columns()[0].name, "price");
            assert_eq!(qr.columns()[0].data_type, DataType::Int32);
            assert_eq!(qr.columns()[1].name, "name");
            assert_eq!(qr.columns()[1].data_type, DataType::String);
            assert!(qr.rows().is_empty());
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_delete_then_absent() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE kv (k BIGINT PRIMARY KEY, v VARCHAR);")
        .unwrap();

    server
        .execute("INSERT INTO kv (k, v) VALUES (1, 'val1');")
        .unwrap();

    // Query exists
    let res = server.execute("SELECT v FROM kv WHERE k = 1;").unwrap();
    match res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("val1".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Delete
    let del_res = server.execute("DELETE FROM kv WHERE k = 1;").unwrap();
    assert_eq!(del_res, StatementResult::dml(1, Some(Version::new(3))));

    // Absent
    let res2 = server.execute("SELECT v FROM kv WHERE k = 1;").unwrap();
    match res2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
            assert_eq!(qr.columns().len(), 1);
            assert_eq!(qr.columns()[0].name, "v");
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_affected_counts_and_commit_versions() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    // DDL: affected = 1, version = None
    let ddl = server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, val VARCHAR);")
        .unwrap();
    assert_eq!(ddl, StatementResult::ddl(1));

    // Single-row INSERT: affected = 1, version = Some(2)
    let ins1 = server
        .execute("INSERT INTO t (id, val) VALUES (1, 'one');")
        .unwrap();
    assert_eq!(ins1, StatementResult::dml(1, Some(Version::new(2))));

    // Multi-row INSERT: affected = 3, version = Some(3)
    let ins_multi = server
        .execute("INSERT INTO t (id, val) VALUES (2, 'two'), (3, 'three'), (4, 'four');")
        .unwrap();
    assert_eq!(ins_multi, StatementResult::dml(3, Some(Version::new(3))));

    // DELETE: affected = 1, version = Some(4)
    let del = server.execute("DELETE FROM t WHERE id = 2;").unwrap();
    assert_eq!(del, StatementResult::dml(1, Some(Version::new(4))));
}

#[test]
fn test_reopen_data_and_next_version() {
    let dir = TempDir::new().unwrap();

    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, val VARCHAR);")
            .unwrap();
        let ins1 = server
            .execute("INSERT INTO t (id, val) VALUES (1, 'alpha');")
            .unwrap();
        assert_eq!(ins1, StatementResult::dml(1, Some(Version::new(2))));
        let ins2 = server
            .execute("INSERT INTO t (id, val) VALUES (2, 'beta');")
            .unwrap();
        assert_eq!(ins2, StatementResult::dml(1, Some(Version::new(3))));
    }

    // Reopen server from disk
    let reopened = LocalServer::open(dir.path()).unwrap();

    // Verify existing rows are visible
    let sel1 = reopened.execute("SELECT val FROM t WHERE id = 1;").unwrap();
    match sel1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("alpha".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let sel2 = reopened.execute("SELECT val FROM t WHERE id = 2;").unwrap();
    match sel2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("beta".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Insert new row after recovery: next commit version must be Some(4)
    let ins3 = reopened
        .execute("INSERT INTO t (id, val) VALUES (3, 'gamma');")
        .unwrap();
    assert_eq!(ins3, StatementResult::dml(1, Some(Version::new(4))));

    // Verify newly inserted row
    let sel3 = reopened.execute("SELECT val FROM t WHERE id = 3;").unwrap();
    match sel3 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("gamma".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_storage_descriptors_dml_and_point_reads_and_unsupported_non_point() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE c_table (id BIGINT PRIMARY KEY, val VARCHAR);")
        .unwrap();

    // Modify the partition storage descriptor to Column in the catalog
    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let mut snap = catalog_store.load().unwrap().unwrap();
    snap.partitions[0].storage = StorageDescriptor::Column;
    snap.generation += 1;
    catalog_store.compare_and_set(1, snap).unwrap();

    // INSERT against Column partition succeeds (rowstore authoritative)
    let ins1 = server
        .execute("INSERT INTO c_table (id, val) VALUES (1, 'a');")
        .unwrap();
    assert_eq!(ins1, StatementResult::dml(1, Some(Version::new(2))));

    // SELECT point read against Column partition succeeds
    let sel1 = server
        .execute("SELECT val FROM c_table WHERE id = 1;")
        .unwrap();
    match sel1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("a".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // DELETE against Column partition succeeds
    let del1 = server.execute("DELETE FROM c_table WHERE id = 1;").unwrap();
    assert_eq!(del1, StatementResult::dml(1, Some(Version::new(3))));

    // Verify deleted
    let absent1 = server
        .execute("SELECT val FROM c_table WHERE id = 1;")
        .unwrap();
    match absent1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Unsupported non-point statements remain rejected under Column
    let err_scan = server.execute("SELECT val FROM c_table;").unwrap_err();
    assert!(matches!(err_scan, HtapError::InvalidArgument(_)));

    let err_unsupported = server
        .execute("SELECT val FROM c_table WHERE id = 1 ORDER BY val;")
        .unwrap_err();
    assert!(matches!(err_unsupported, HtapError::Unsupported(_)));

    // Modify to Converting storage with valid conversion metadata
    let mut snap2 = catalog_store.load().unwrap().unwrap();
    let conv_gen = snap2.generation + 1;
    snap2.generation = conv_gen;
    snap2.partitions[0].generation = conv_gen;
    snap2.partitions[0].storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: conv_gen,
    };
    snap2.partitions[0].conversion = Some(ConversionDescriptor::new(
        conv_gen,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(3),
        ConversionPhase::SnapshotPinned,
    ));
    catalog_store.compare_and_set(2, snap2).unwrap();

    // INSERT against Converting partition succeeds (rowstore authoritative)
    let ins2 = server
        .execute("INSERT INTO c_table (id, val) VALUES (2, 'b');")
        .unwrap();
    assert_eq!(ins2, StatementResult::dml(1, Some(Version::new(4))));

    // SELECT point read against Converting partition succeeds
    let sel2 = server
        .execute("SELECT val FROM c_table WHERE id = 2;")
        .unwrap();
    match sel2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("b".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // DELETE against Converting partition succeeds
    let del2 = server.execute("DELETE FROM c_table WHERE id = 2;").unwrap();
    assert_eq!(del2, StatementResult::dml(1, Some(Version::new(5))));

    // Verify deleted
    let absent2 = server
        .execute("SELECT val FROM c_table WHERE id = 2;")
        .unwrap();
    match absent2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Unsupported non-point statements remain rejected under Converting
    let err_scan2 = server.execute("SELECT val FROM c_table;").unwrap_err();
    assert!(matches!(err_scan2, HtapError::InvalidArgument(_)));

    let err_unsupported2 = server
        .execute("SELECT val FROM c_table WHERE id = 2 ORDER BY val;")
        .unwrap_err();
    assert!(matches!(err_unsupported2, HtapError::Unsupported(_)));
}

#[test]
fn test_server_data_mover_integration() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    // 1. Create SQL table
    let ddl_res = server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT);")
        .unwrap();
    assert_eq!(ddl_res, StatementResult::ddl(1));

    // 2. Obtain known local IDs (table/partition/tablet 1)
    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snap = catalog_store
        .load()
        .unwrap()
        .expect("catalog snapshot must exist");
    let table = snap.table_by_name("users").unwrap();
    let table_id = table.id;
    let partition_id = table.partitions[0];
    let partition = snap.partition(partition_id).unwrap();
    let tablet_id = partition.tablets[0];

    assert_eq!(table_id, TableId::new(1));
    assert_eq!(partition_id.as_u64(), 1);
    assert_eq!(tablet_id, TabletId::new(1));

    // 3. Import CSV through server.data_mover using the same server state
    let csv_path = dir.path().join("users.csv");
    std::fs::write(&csv_path, "id,name,age\n1,Alice,30\n2,Bob,25\n").unwrap();

    let copy_opts = CopyOptions::new(
        "import_users_1",
        table_id,
        tablet_id,
        DataFormat::Csv,
        &csv_path,
    );
    let report = server.data_mover().copy_from_csv(&copy_opts).unwrap();
    assert_eq!(report.records_read, 2);
    assert_eq!(report.records_committed, 2);
    assert_eq!(report.rows_written, 2);
    assert_eq!(report.records_skipped, 0);

    // 4. Point-select through server.execute
    let sel1 = server
        .execute("SELECT name, age FROM users WHERE id = 1;")
        .unwrap();
    match sel1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Alice".into())));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int32(30)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let sel2 = server
        .execute("SELECT name, age FROM users WHERE id = 2;")
        .unwrap();
    match sel2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Bob".into())));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int32(25)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // 5. Verify movement job exists under root/movement
    let job_file = dir
        .path()
        .join("movement")
        .join("jobs")
        .join("import_users_1")
        .join("JOB");
    assert!(
        job_file.is_file(),
        "movement job file must exist under root/movement"
    );

    let job = server
        .data_mover()
        .load_job("import_users_1")
        .unwrap()
        .expect("job must be loadable via server data_mover");
    assert_eq!(job.phase, MovementJobPhase::Complete);
    assert_eq!(job.counters.records_committed, 2);

    // 6. Drop/reopen server and verify catalog/data/job persistence
    drop(server);

    let reopened = LocalServer::open(dir.path()).unwrap();

    // Verify catalog persistence
    let snap_reopened = LocalCatalogStore::open(dir.path().join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    assert!(snap_reopened.table_by_name("users").is_some());

    // Verify data persistence via point-select on reopened server
    let sel1_reopened = reopened
        .execute("SELECT name, age FROM users WHERE id = 1;")
        .unwrap();
    match sel1_reopened {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Alice".into())));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int32(30)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Verify job persistence via reopened server data_mover
    let job_reopened = reopened
        .data_mover()
        .load_job("import_users_1")
        .unwrap()
        .expect("movement job must persist across reopen");
    assert_eq!(job_reopened.phase, MovementJobPhase::Complete);
    assert_eq!(job_reopened.counters.records_committed, 2);

    // 7. Perform server DML and verify commit/version continuity where observable
    // Prior movement batch committed at Version 2; subsequent DML commits at Version 3.
    let insert_res = reopened
        .execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35);")
        .unwrap();
    assert_eq!(insert_res, StatementResult::dml(1, Some(Version::new(3))));

    let sel3 = reopened
        .execute("SELECT name, age FROM users WHERE id = 3;")
        .unwrap();
    match sel3 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Charlie".into())));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int32(35)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // 8. Verify export and JSONL reader import on reopened server with version continuity
    let export_csv_path = dir.path().join("export_users.csv");
    let export_opts = CopyOptions::new(
        "export_users_1",
        table_id,
        tablet_id,
        DataFormat::Csv,
        &export_csv_path,
    );
    let exp_report = reopened.data_mover().copy_to_csv(&export_opts).unwrap();
    assert_eq!(exp_report.records_read, 3);
    assert_eq!(exp_report.rows_written, 3);

    let exported_csv = std::fs::read_to_string(&export_csv_path).unwrap();
    assert!(exported_csv.contains("Alice"));
    assert!(exported_csv.contains("Bob"));
    assert!(exported_csv.contains("Charlie"));

    // JSONL reader import commits batch at Version 4
    let jsonl_data = "{\"id\":4,\"name\":\"Dana\",\"age\":28}\n";
    let jsonl_opts = CopyOptions::new(
        "import_users_jsonl",
        table_id,
        tablet_id,
        DataFormat::JsonLines,
        dir.path().join("ignored.jsonl"),
    );
    let jsonl_report = reopened
        .data_mover()
        .copy_from_jsonl_reader(&jsonl_opts, jsonl_data.as_bytes())
        .unwrap();
    assert_eq!(jsonl_report.records_read, 1);
    assert_eq!(jsonl_report.records_committed, 1);

    let sel4 = reopened
        .execute("SELECT name, age FROM users WHERE id = 4;")
        .unwrap();
    match sel4 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Dana".into())));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int32(28)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Subsequent DML after JSONL batch commits at Version 5
    let delete_res = reopened.execute("DELETE FROM users WHERE id = 4;").unwrap();
    assert_eq!(delete_res, StatementResult::dml(1, Some(Version::new(5))));
}

#[test]
fn test_server_data_mover_clone_verify_repair() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    server
        .execute("CREATE TABLE docs (id BIGINT PRIMARY KEY, body VARCHAR);")
        .unwrap();

    server
        .execute("INSERT INTO docs (id, body) VALUES (1, 'first'), (2, 'second');")
        .unwrap();

    // Register an unhealthy follower replica in tablet 1
    let catalog_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let mut snap = catalog_store.load().unwrap().unwrap();
    let follower_rep_id = ReplicaId::new(2);
    snap.replicas.push(ReplicaDescriptor::new(
        follower_rep_id,
        TabletId::new(1),
        NodeId::new(2),
        false, // is_leader
        false, // healthy
        snap.generation + 1,
    ));
    snap.tablets[0].replicas.push(follower_rep_id);
    snap.generation += 1;
    catalog_store.compare_and_set(1, snap).unwrap();

    let clone_opts = TabletCloneOptions::new("clone_docs_1", TabletId::new(1), follower_rep_id);

    // 1. Clone tablet via server data_mover
    let manifest = server.data_mover().clone_tablet(&clone_opts).unwrap();
    assert_eq!(manifest.job_id, "clone_docs_1");
    assert_eq!(manifest.source_tablet_id, TabletId::new(1));
    assert_eq!(manifest.target_replica_id, follower_rep_id);
    assert_eq!(manifest.row_count, 2);

    // 2. Verify clone package via server data_mover
    let verified = server.data_mover().verify_package(&clone_opts).unwrap();
    assert_eq!(verified.row_count, 2);
    assert_eq!(verified.payload_checksum, manifest.payload_checksum);

    // 3. Repair replica via server data_mover
    let repaired = server.data_mover().repair_tablet(&clone_opts).unwrap();
    assert_eq!(repaired.id, follower_rep_id);
    assert!(repaired.healthy);

    // Verify catalog reflects repaired healthy status
    let updated_snap = catalog_store.load().unwrap().unwrap();
    let rep = updated_snap.replica(follower_rep_id).unwrap();
    assert!(rep.healthy);
}

fn child_binary() -> std::path::PathBuf {
    let mut dir = std::env::current_exe().expect("test executable path");
    dir.pop(); // .../target/<profile>/deps
    if dir.ends_with("deps") {
        dir.pop(); // .../target/<profile>
    }
    let exe = format!("server_lock_child{}", std::env::consts::EXE_SUFFIX);
    let candidate = dir.join(&exe);
    if !candidate.is_file() {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "htap-server", "--bin", "server_lock_child"])
            .status()
            .expect("building server_lock_child");
        assert!(status.success(), "cargo build server_lock_child failed");
    }
    assert!(
        candidate.is_file(),
        "server_lock_child binary not found at {}",
        candidate.display()
    );
    candidate
}

#[test]
fn test_subprocess_exclusive_lock_contention_and_symlink() {
    use std::io::BufRead;

    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // 1. Spawn child holding lock
    let mut child = std::process::Command::new(child_binary())
        .arg(root)
        .arg("hold")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn server_lock_child");

    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read from child");
    assert_eq!(line.trim(), "LOCKED");

    // 2. Child is holding lock. Opening from parent process must fail with Conflict.
    let err = match LocalServer::open(root) {
        Err(e) => e,
        Ok(_) => panic!("expected LocalServer::open to fail with Conflict"),
    };
    assert!(
        matches!(err, HtapError::Conflict(_)),
        "expected Conflict error, got {err:?}"
    );
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("exclusive root lock contention"),
        "error message did not mention contention: {err_msg}"
    );
    assert!(
        err_msg.contains(&format!("pid={}", child.id())),
        "error message should contain child pid: {err_msg}"
    );

    // 3. Symlink alias test where supported: opening via symlink must also fail with Conflict
    #[cfg(unix)]
    {
        let symlink_parent = TempDir::new().unwrap();
        let symlink_path = symlink_parent.path().join("server_symlink_alias");
        std::os::unix::fs::symlink(root, &symlink_path).unwrap();

        let sym_err = match LocalServer::open(&symlink_path) {
            Err(e) => e,
            Ok(_) => panic!("expected LocalServer::open on symlink to fail with Conflict"),
        };
        assert!(
            matches!(sym_err, HtapError::Conflict(_)),
            "expected Conflict on symlink open, got {sym_err:?}"
        );
        let sym_err_msg = sym_err.to_string();
        assert!(
            sym_err_msg.contains("exclusive root lock contention"),
            "symlink error message: {sym_err_msg}"
        );
    }

    // 4. Second child opening same root must also fail with Conflict (exit code 42)
    let child2_output = std::process::Command::new(child_binary())
        .arg(root)
        .arg("try_once")
        .output()
        .expect("spawn second child");
    assert_eq!(
        child2_output.status.code(),
        Some(42),
        "second child should exit with Conflict status code 42"
    );

    // 5. Release child lock by dropping its stdin (closing stream) and waiting for exit
    drop(child.stdin.take());
    let status = child.wait().expect("wait on child");
    assert!(status.success(), "child did not exit cleanly: {status:?}");

    // 6. After child exits, reopen succeeds and operates normally
    let server = LocalServer::open(root).expect("reopen after child exit should succeed");
    let res = server
        .execute("CREATE TABLE test_tbl (id BIGINT PRIMARY KEY, v VARCHAR);")
        .expect("statement should succeed");
    assert!(matches!(res, StatementResult::Command(_)));
}

#[test]
fn test_wal_gc_recovery_with_transaction_manager() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // 1. Open LocalServer, create table and insert rows
    {
        let server = LocalServer::open(root).unwrap();
        server
            .execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, name VARCHAR, balance INT);")
            .unwrap();
        server
            .execute("INSERT INTO accounts (id, name, balance) VALUES (1, 'Alice', 100);")
            .unwrap();
        server
            .execute("INSERT INTO accounts (id, name, balance) VALUES (2, 'Bob', 200);")
            .unwrap();
    }

    // 2. Open underlying rowstore Engine directly (since server was dropped and lock released),
    // and force a flush which writes an SST with the manifest ledger and executes WAL checkpoint & GC.
    {
        let rowstore_dir = root.join("rowstore");
        let engine =
            htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(rowstore_dir)).unwrap();
        engine.flush().unwrap();
    }

    // 3. Reopen LocalServer.
    // During LocalServer::open(), txn_manager.recover() replays txn.journal and calls
    // RowstoreParticipant::apply for all durable committed transactions in txn.journal.
    // With the manifest ledger, rowstore accepts idempotent reapply cleanly even though
    // its active memtable was flushed and WAL checkpointed.
    let server2 =
        LocalServer::open(root).expect("LocalServer reopen with GC recovery must succeed");

    // 4. Verify data is intact and queries return expected results
    let res = server2
        .execute("SELECT name, balance FROM accounts WHERE id = 1;")
        .unwrap();
    match res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::String("Alice".to_string())));
            assert_eq!(row.get(1), Some(&Value::Int32(100)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // 5. Subsequent transactions succeed normally
    let insert_res = server2
        .execute("INSERT INTO accounts (id, name, balance) VALUES (3, 'Charlie', 300);")
        .unwrap();
    assert!(matches!(
        insert_res,
        StatementResult::Command(CommandResult::Dml { affected: 1, .. })
    ));

    let res_all = server2
        .execute("SELECT balance FROM accounts WHERE id = 3;")
        .unwrap();
    match res_all {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int32(300)));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}
