//! Comprehensive integration tests for `LocalServer`.

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    ConversionDescriptor, ConversionPhase, NodeId, PartitionId, PartitioningDescriptor,
    PartitioningMethod, RangeBound, ReplicaDescriptor, ReplicaId, StorageDescriptor, StorageFormat,
    TableId, TabletId,
};
use htap_common::types::{ColumnDef, DataType, Schema, Value};
use htap_common::version::Version;
use htap_common::HtapError;
use htap_movement::{CopyOptions, DataFormat, MovementJobPhase, TabletCloneOptions};
use htap_server::{
    ListPartitionDefinition, LocalServer, PartitionTopology, PartitionedTableDefinition,
    RangePartitionDefinition,
};
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

    // Analytical queries against Column partition without manifest are rejected
    let err_scan = server.execute("SELECT val FROM c_table;").unwrap_err();
    assert!(matches!(err_scan, HtapError::InvalidArgument(_)));
    assert!(err_scan.to_string().contains("has no column manifest"));

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

    // Converting partition in SnapshotPinned phase without manifest uses rowstore fallback
    let scan2 = server.execute("SELECT val FROM c_table;").unwrap();
    match scan2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    server
        .execute("INSERT INTO c_table (id, val) VALUES (3, 'c');")
        .unwrap();
    let scan3 = server.execute("SELECT val FROM c_table;").unwrap();
    match scan3 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("c".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

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

#[test]
fn test_create_table_overflow_typed_errors() {
    // 1. Generation overflow
    {
        let dir = TempDir::new().unwrap();
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t1 (id INT PRIMARY KEY);")
            .unwrap();

        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let snap = cat_store.load().unwrap().unwrap();
        let mut snap_gen_max = snap.clone();
        snap_gen_max.generation = u64::MAX;
        cat_store
            .compare_and_set(snap.generation, snap_gen_max)
            .unwrap();

        let err_gen = server
            .execute("CREATE TABLE t2 (id INT PRIMARY KEY);")
            .unwrap_err();
        assert!(matches!(
            err_gen,
            HtapError::CounterOverflow {
                counter: "catalog_generation"
            }
        ));
    }

    // 2. TableId overflow
    {
        let dir = TempDir::new().unwrap();
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t1 (id INT PRIMARY KEY);")
            .unwrap();

        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let snap = cat_store.load().unwrap().unwrap();
        let mut snap_tbl_max = snap.clone();
        snap_tbl_max.generation = snap.generation + 1;
        snap_tbl_max.tables[0].id = TableId::new(u64::MAX);
        snap_tbl_max.partitions[0].table_id = TableId::new(u64::MAX);
        cat_store
            .compare_and_set(snap.generation, snap_tbl_max)
            .unwrap();

        let err_tbl = server
            .execute("CREATE TABLE t2 (id INT PRIMARY KEY);")
            .unwrap_err();
        assert!(matches!(
            err_tbl,
            HtapError::CounterOverflow {
                counter: "table_id"
            }
        ));
    }

    // 3. PartitionId overflow
    {
        let dir = TempDir::new().unwrap();
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t1 (id INT PRIMARY KEY);")
            .unwrap();

        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let snap = cat_store.load().unwrap().unwrap();
        let mut snap_part_max = snap.clone();
        snap_part_max.generation = snap.generation + 1;
        snap_part_max.partitions[0].id = PartitionId::new(u64::MAX);
        snap_part_max.tables[0].partitions = vec![PartitionId::new(u64::MAX)];
        snap_part_max.tablets[0].partition_id = PartitionId::new(u64::MAX);
        cat_store
            .compare_and_set(snap.generation, snap_part_max)
            .unwrap();

        let err_part = server
            .execute("CREATE TABLE t2 (id INT PRIMARY KEY);")
            .unwrap_err();
        assert!(matches!(
            err_part,
            HtapError::CounterOverflow {
                counter: "partition_id"
            }
        ));
    }

    // 4. TabletId overflow
    {
        let dir = TempDir::new().unwrap();
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t1 (id INT PRIMARY KEY);")
            .unwrap();

        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let snap = cat_store.load().unwrap().unwrap();
        let mut snap_tab_max = snap.clone();
        snap_tab_max.generation = snap.generation + 1;
        snap_tab_max.tablets[0].id = TabletId::new(u64::MAX);
        snap_tab_max.partitions[0].tablets = vec![TabletId::new(u64::MAX)];
        snap_tab_max.replicas[0].tablet_id = TabletId::new(u64::MAX);
        cat_store
            .compare_and_set(snap.generation, snap_tab_max)
            .unwrap();

        let err_tab = server
            .execute("CREATE TABLE t2 (id INT PRIMARY KEY);")
            .unwrap_err();
        assert!(matches!(
            err_tab,
            HtapError::CounterOverflow {
                counter: "tablet_id"
            }
        ));
    }

    // 5. ReplicaId overflow
    {
        let dir = TempDir::new().unwrap();
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t1 (id INT PRIMARY KEY);")
            .unwrap();

        let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
        let snap = cat_store.load().unwrap().unwrap();
        let mut snap_rep_max = snap.clone();
        snap_rep_max.generation = snap.generation + 1;
        snap_rep_max.replicas[0].id = ReplicaId::new(u64::MAX);
        snap_rep_max.tablets[0].replicas = vec![ReplicaId::new(u64::MAX)];
        cat_store
            .compare_and_set(snap.generation, snap_rep_max)
            .unwrap();

        let err_rep = server
            .execute("CREATE TABLE t2 (id INT PRIMARY KEY);")
            .unwrap_err();
        assert!(matches!(
            err_rep,
            HtapError::CounterOverflow {
                counter: "replica_id"
            }
        ));
    }
}

#[test]
fn test_analytic_row_scan_and_filters() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT, bio VARCHAR, score DOUBLE);")
        .unwrap();

    server
        .execute("INSERT INTO users (id, name, age, bio, score) VALUES (1, 'Alice', 30, 'engineer', 95.5);")
        .unwrap();
    server
        .execute("INSERT INTO users (id, name, age, bio, score) VALUES (2, 'Bob', 25, NULL, 80.0);")
        .unwrap();
    server
        .execute("INSERT INTO users (id, name, age, bio, score) VALUES (3, 'Charlie', 35, 'designer', 88.0);")
        .unwrap();
    server
        .execute(
            "INSERT INTO users (id, name, age, bio, score) VALUES (4, 'Dave', 40, NULL, 72.5);",
        )
        .unwrap();
    server
        .execute(
            "INSERT INTO users (id, name, age, bio, score) VALUES (5, 'Eve', 25, 'lead', 99.0);",
        )
        .unwrap();

    // 1. Point route remains completely untouched and independent
    let point_res = server
        .execute("SELECT name FROM users WHERE id = 1;")
        .unwrap();
    match point_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Alice".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 2. Full table scan with specific projection
    let scan_res = server.execute("SELECT id, name, age FROM users;").unwrap();
    match scan_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 5);
            assert_eq!(qr.columns().len(), 3);
            assert_eq!(qr.columns()[0].name, "id");
            assert_eq!(qr.columns()[1].name, "name");
            assert_eq!(qr.columns()[2].name, "age");
            // Deterministic PK order: 1, 2, 3, 4, 5
            for i in 0..5 {
                assert_eq!(qr.rows()[i].get(0), Some(&Value::Int64((i + 1) as i64)));
            }
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 3. Full table scan with wildcard SELECT *
    let wildcard_res = server.execute("SELECT * FROM users;").unwrap();
    match wildcard_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 5);
            assert_eq!(qr.columns().len(), 5);
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 4. Conjunction of comparison filters: age >= 25 AND age <= 35 AND id != 2
    // Matching rows: (1, Alice, 30), (3, Charlie, 35), (5, Eve, 25)
    let filter_res = server
        .execute("SELECT name, age FROM users WHERE age >= 25 AND age <= 35 AND id != 2;")
        .unwrap();
    match filter_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Alice".into())));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::String("Charlie".into())));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::String("Eve".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 5. Filter with IS NULL: bio IS NULL
    // Matching rows: (2, Bob), (4, Dave)
    let null_res = server
        .execute("SELECT id, name FROM users WHERE bio IS NULL;")
        .unwrap();
    match null_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(2)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(4)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 6. Filter with IS NOT NULL: bio IS NOT NULL
    // Matching rows: (1, Alice), (3, Charlie), (5, Eve)
    let not_null_res = server
        .execute("SELECT id, name FROM users WHERE bio IS NOT NULL;")
        .unwrap();
    match not_null_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(1)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(3)));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(5)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 7. Filter resulting in 0 matching rows returns empty QueryResult with valid schema
    let empty_res = server
        .execute("SELECT id, name FROM users WHERE age > 100;")
        .unwrap();
    match empty_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
            assert_eq!(qr.columns().len(), 2);
            assert_eq!(qr.columns()[0].name, "id");
            assert_eq!(qr.columns()[1].name, "name");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_analytic_aggregates_nulls_and_empty() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, age INT, bio VARCHAR, score DOUBLE);")
        .unwrap();

    // 1. Global aggregation on EMPTY table:
    // COUNT(*) -> 0, COUNT(col) -> 0, SUM -> NULL, MIN -> NULL, MAX -> NULL
    // Returns exactly one row.
    let empty_agg = server
        .execute("SELECT COUNT(*), COUNT(age), SUM(age), MIN(age), MAX(age) FROM users;")
        .unwrap();
    match empty_agg {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(0)));
            assert_eq!(row.get(1), Some(&Value::Int64(0)));
            assert_eq!(row.get(2), Some(&Value::Null));
            assert_eq!(row.get(3), Some(&Value::Null));
            assert_eq!(row.get(4), Some(&Value::Null));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Insert rows with mixed NULLs
    server
        .execute("INSERT INTO users (id, age, bio, score) VALUES (1, 30, 'eng', 10.5);")
        .unwrap();
    server
        .execute("INSERT INTO users (id, age, bio, score) VALUES (2, NULL, NULL, 20.5);")
        .unwrap();
    server
        .execute("INSERT INTO users (id, age, bio, score) VALUES (3, 20, NULL, 30.0);")
        .unwrap();

    // 2. Aggregates with null handling
    let agg_res = server
        .execute(
            "SELECT COUNT(*), COUNT(age), SUM(age), MIN(age), MAX(age), SUM(score) FROM users;",
        )
        .unwrap();
    match agg_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(3))); // COUNT(*)
            assert_eq!(row.get(1), Some(&Value::Int64(2))); // COUNT(age) skips row 2
            assert_eq!(row.get(2), Some(&Value::Int64(50))); // SUM(age) = 30 + 20
            assert_eq!(row.get(3), Some(&Value::Int32(20))); // MIN(age) = 20
            assert_eq!(row.get(4), Some(&Value::Int32(30))); // MAX(age) = 30
            assert_eq!(row.get(5), Some(&Value::Float64(61.0))); // SUM(score) = 10.5 + 20.5 + 30.0
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 3. All-NULL column aggregation
    let all_null_agg = server
        .execute("SELECT COUNT(bio), MIN(bio), MAX(bio) FROM users WHERE age IS NULL;")
        .unwrap();
    match all_null_agg {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(0)));
            assert_eq!(row.get(1), Some(&Value::Null));
            assert_eq!(row.get(2), Some(&Value::Null));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 4. Arithmetic overflow on SUM returns clear InvalidArgument error
    server
        .execute("CREATE TABLE overflow_test (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    server
        .execute(&format!(
            "INSERT INTO overflow_test (id, val) VALUES (1, {});",
            i64::MAX
        ))
        .unwrap();
    server
        .execute("INSERT INTO overflow_test (id, val) VALUES (2, 1);")
        .unwrap();

    let err_overflow = server
        .execute("SELECT SUM(val) FROM overflow_test;")
        .unwrap_err();
    assert!(
        matches!(err_overflow, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {err_overflow:?}"
    );
    assert!(err_overflow.to_string().contains("overflow"));
}

#[test]
fn test_analytic_group_by_and_null_group() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute(
            "CREATE TABLE items (id BIGINT PRIMARY KEY, category VARCHAR, qty INT, price DOUBLE);",
        )
        .unwrap();

    // 1. Empty table with GROUP BY returns ZERO rows
    let empty_grouped = server
        .execute("SELECT category, COUNT(*), SUM(qty) FROM items GROUP BY category;")
        .unwrap();
    match empty_grouped {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
            assert_eq!(qr.columns().len(), 3);
            assert_eq!(qr.columns()[0].name, "category");
            assert_eq!(qr.columns()[1].name, "COUNT(*)");
            assert_eq!(qr.columns()[2].name, "SUM(qty)");
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Insert rows with multiple groups, including NULL category
    server
        .execute("INSERT INTO items (id, category, qty, price) VALUES (1, 'book', 2, 15.0);")
        .unwrap();
    server
        .execute("INSERT INTO items (id, category, qty, price) VALUES (2, 'book', 3, 20.0);")
        .unwrap();
    server
        .execute("INSERT INTO items (id, category, qty, price) VALUES (3, 'food', 5, 5.0);")
        .unwrap();
    server
        .execute("INSERT INTO items (id, category, qty, price) VALUES (4, NULL, 1, 10.0);")
        .unwrap();
    server
        .execute("INSERT INTO items (id, category, qty, price) VALUES (5, NULL, 4, 12.0);")
        .unwrap();

    // 2. Grouped aggregation with deterministic BTreeMap ordering and SQL NULL group
    // In Value order: NULL comes before non-null strings ('book', 'food').
    let grouped_res = server
        .execute("SELECT category, COUNT(*), COUNT(category), SUM(qty), MIN(price), MAX(price) FROM items GROUP BY category;")
        .unwrap();
    match grouped_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);

            // Group 1: NULL category
            let r0 = &qr.rows()[0];
            assert_eq!(r0.get(0), Some(&Value::Null));
            assert_eq!(r0.get(1), Some(&Value::Int64(2))); // COUNT(*) = 2
            assert_eq!(r0.get(2), Some(&Value::Int64(0))); // COUNT(category) = 0
            assert_eq!(r0.get(3), Some(&Value::Int64(5))); // SUM(qty) = 1 + 4 = 5
            assert_eq!(r0.get(4), Some(&Value::Float64(10.0))); // MIN(price)
            assert_eq!(r0.get(5), Some(&Value::Float64(12.0))); // MAX(price)

            // Group 2: 'book'
            let r1 = &qr.rows()[1];
            assert_eq!(r1.get(0), Some(&Value::String("book".into())));
            assert_eq!(r1.get(1), Some(&Value::Int64(2)));
            assert_eq!(r1.get(2), Some(&Value::Int64(2)));
            assert_eq!(r1.get(3), Some(&Value::Int64(5))); // 2 + 3 = 5
            assert_eq!(r1.get(4), Some(&Value::Float64(15.0)));
            assert_eq!(r1.get(5), Some(&Value::Float64(20.0)));

            // Group 3: 'food'
            let r2 = &qr.rows()[2];
            assert_eq!(r2.get(0), Some(&Value::String("food".into())));
            assert_eq!(r2.get(1), Some(&Value::Int64(1)));
            assert_eq!(r2.get(2), Some(&Value::Int64(1)));
            assert_eq!(r2.get(3), Some(&Value::Int64(5)));
            assert_eq!(r2.get(4), Some(&Value::Float64(5.0)));
            assert_eq!(r2.get(5), Some(&Value::Float64(5.0)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 3. Filter with GROUP BY resulting in 0 matches returns 0 rows
    let zero_group = server
        .execute("SELECT category, COUNT(*) FROM items WHERE qty > 100 GROUP BY category;")
        .unwrap();
    match zero_group {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_analytic_unsupported_clauses() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, age INT);")
        .unwrap();

    // ORDER BY is unsupported
    let err_order = server
        .execute("SELECT * FROM users ORDER BY age;")
        .unwrap_err();
    assert!(matches!(err_order, HtapError::Unsupported(_)));

    // LIMIT is unsupported
    let err_limit = server.execute("SELECT * FROM users LIMIT 10;").unwrap_err();
    assert!(matches!(err_limit, HtapError::Unsupported(_)));

    // JOIN is unsupported
    let err_join = server
        .execute("SELECT * FROM users u1 JOIN users u2 ON u1.id = u2.id;")
        .unwrap_err();
    assert!(
        matches!(err_join, HtapError::Unsupported(_))
            || matches!(err_join, HtapError::InvalidArgument(_))
    );

    // AVG is unsupported
    let err_avg = server.execute("SELECT AVG(age) FROM users;").unwrap_err();
    assert!(matches!(err_avg, HtapError::Unsupported(_)));

    // Arithmetic expressions in projection unsupported
    let err_expr = server.execute("SELECT age + 1 FROM users;").unwrap_err();
    assert!(matches!(err_expr, HtapError::Unsupported(_)));
}

#[test]
fn test_analytic_materialized_column_base_plus_delta() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, val VARCHAR, num INT);")
        .unwrap();

    // Commit 3 initial rows in Row storage
    server
        .execute("INSERT INTO t (id, val, num) VALUES (1, 'v1', 10);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, val, num) VALUES (2, 'v2', 20);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, val, num) VALUES (3, 'v3', 30);")
        .unwrap();

    // Convert table to columnar format using server's colstore directory
    let manifest = server.convert_table("t").unwrap();
    assert_eq!(manifest.total_rows(), 3);
    assert_eq!(manifest.segment_count(), 1);

    // Verify analytical query reads base columnar rows
    let scan1 = server.execute("SELECT * FROM t;").unwrap();
    match scan1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(1)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(2)));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(3)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let agg1 = server.execute("SELECT COUNT(*), SUM(num) FROM t;").unwrap();
    match agg1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(3)));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int64(60)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Now apply post-base deltas in rowstore:
    // 1. Delete key 1
    // 2. Update key 2
    // 3. Insert key 4
    server.execute("DELETE FROM t WHERE id = 1;").unwrap();
    server
        .execute("INSERT INTO t (id, val, num) VALUES (2, 'v2_updated', 25);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, val, num) VALUES (4, 'v4', 40);")
        .unwrap();

    // Verify point read route remains untouched and authoritative
    let point_del = server.execute("SELECT val FROM t WHERE id = 1;").unwrap();
    match point_del {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
    let point_upd = server.execute("SELECT val FROM t WHERE id = 2;").unwrap();
    match point_upd {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("v2_updated".into()))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Verify analytical scan overlays base columnar rows with rowstore deltas:
    // Keys remaining: 2 (updated), 3 (base), 4 (new)
    // Deterministic PK order: 2, 3, 4
    let scan2 = server.execute("SELECT id, val, num FROM t;").unwrap();
    match scan2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(2)));
            assert_eq!(
                qr.rows()[0].get(1),
                Some(&Value::String("v2_updated".into()))
            );
            assert_eq!(qr.rows()[0].get(2), Some(&Value::Int32(25)));

            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(3)));
            assert_eq!(qr.rows()[1].get(1), Some(&Value::String("v3".into())));
            assert_eq!(qr.rows()[1].get(2), Some(&Value::Int32(30)));

            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(4)));
            assert_eq!(qr.rows()[2].get(1), Some(&Value::String("v4".into())));
            assert_eq!(qr.rows()[2].get(2), Some(&Value::Int32(40)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Verify aggregation over base + deltas
    let agg2 = server
        .execute("SELECT COUNT(*), SUM(num), MIN(num), MAX(num) FROM t;")
        .unwrap();
    match agg2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(3))); // 3 rows
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int64(95))); // 25 + 30 + 40 = 95
            assert_eq!(qr.rows()[0].get(2), Some(&Value::Int32(25)));
            assert_eq!(qr.rows()[0].get(3), Some(&Value::Int32(40)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Verify filter over base + deltas
    let filter2 = server
        .execute("SELECT val FROM t WHERE num >= 30;")
        .unwrap();
    match filter2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("v3".into())));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::String("v4".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_analytic_row_vs_column_base_plus_delta_equivalence() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    // Create Row reference table and Column target table
    server
        .execute("CREATE TABLE t_row (id BIGINT PRIMARY KEY, category VARCHAR, price BIGINT, rating INT);")
        .unwrap();
    server
        .execute("CREATE TABLE t_col (id BIGINT PRIMARY KEY, category VARCHAR, price BIGINT, rating INT);")
        .unwrap();

    let insert_initial = |tbl: &str| {
        vec![
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (1, 'tech', 100, 5);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (2, 'tech', 200, 4);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (3, 'books', 50, 5);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (4, NULL, 30, 3);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (5, 'books', 80, NULL);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (6, 'furniture', 500, 2);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (7, 'tech', 150, 4);"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (8, 'furniture', 300, NULL);"),
        ]
    };

    for sql in insert_initial("t_row") {
        server.execute(&sql).unwrap();
    }
    for sql in insert_initial("t_col") {
        server.execute(&sql).unwrap();
    }

    // Convert t_col to Column storage
    server.convert_table("t_col").unwrap();

    // Apply identical mutation sequences post-base to both tables:
    // 1. Update key 2 (tech 200 -> tech 220)
    // 2. Delete key 3 (books 50)
    // 3. Insert key 9 (tech 400, 5)
    // 4. Insert then delete key 10
    // 5. Update key 5 (books 80 -> NULL 85)
    let mutations = |tbl: &str| {
        vec![
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (2, 'tech', 220, 4);"),
            format!("DELETE FROM {tbl} WHERE id = 3;"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (9, 'tech', 400, 5);"),
            format!(
                "INSERT INTO {tbl} (id, category, price, rating) VALUES (10, 'other', 999, 1);"
            ),
            format!("DELETE FROM {tbl} WHERE id = 10;"),
            format!("INSERT INTO {tbl} (id, category, price, rating) VALUES (5, NULL, 85, NULL);"),
        ]
    };

    for sql in mutations("t_row") {
        server.execute(&sql).unwrap();
    }
    for sql in mutations("t_col") {
        server.execute(&sql).unwrap();
    }

    // List of equivalent analytical queries:
    let queries = [
        // 1. Plain scan with projection & reordering
        "SELECT category, id FROM {tbl};",
        // 2. Projection subset (single column)
        "SELECT price FROM {tbl};",
        // 3. Unprojected filter and NotEq (!=)
        "SELECT id, price FROM {tbl} WHERE category != 'tech';",
        // 4. Null predicate: IS NULL
        "SELECT id, price FROM {tbl} WHERE category IS NULL;",
        // 5. Null predicate: IS NOT NULL
        "SELECT id, category FROM {tbl} WHERE rating IS NOT NULL;",
        // 6. Compound filter (AND with range and NotEq)
        "SELECT id, category, price FROM {tbl} WHERE price >= 100 AND category != 'furniture';",
        // 7. Global aggregates (COUNT, SUM, MIN, MAX, nullable columns)
        "SELECT COUNT(*), COUNT(rating), COUNT(category), SUM(price), MIN(price), MAX(price) FROM {tbl};",
        // 8. Grouped aggregation on nullable column
        "SELECT category, COUNT(*), SUM(price) FROM {tbl} GROUP BY category;",
        // 9. Grouped aggregation with filter
        "SELECT category, COUNT(*), SUM(price) FROM {tbl} WHERE price > 50 GROUP BY category;",
        // 10. Filter matching no rows with global aggregates
        "SELECT COUNT(*), SUM(price), MIN(price) FROM {tbl} WHERE price > 10000;",
    ];

    for q_template in queries {
        let sql_row = q_template.replace("{tbl}", "t_row");
        let sql_col = q_template.replace("{tbl}", "t_col");

        let res_row = match server.execute(&sql_row).unwrap() {
            StatementResult::Query(qr) => qr,
            other => panic!("expected query result, got {other:?}"),
        };
        let res_col = match server.execute(&sql_col).unwrap() {
            StatementResult::Query(qr) => qr,
            other => panic!("expected query result, got {other:?}"),
        };

        assert_eq!(
            res_row.columns(),
            res_col.columns(),
            "columns mismatch for query: {q_template}"
        );
        assert_eq!(
            res_row.rows(),
            res_col.rows(),
            "rows mismatch for query: {q_template}"
        );
    }
}

#[test]
fn test_pushdown_predicate_selection_and_pruning_stats() {
    use htap_server::olap::{plan_source_columns, select_pushdown_predicate};
    use htap_sql::{bind, parse_one};

    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR, price BIGINT);")
        .unwrap();

    let cat_snap = LocalCatalogStore::open(dir.path().join("catalog"))
        .unwrap()
        .load()
        .unwrap()
        .unwrap();

    // 1. NotEq must NEVER be pushed down
    let ast1 = parse_one("SELECT name FROM items WHERE price != 100;").unwrap();
    let bound1 = match bind(&ast1, &cat_snap).unwrap() {
        htap_sql::BoundStatement::AnalyticSelect(sel) => sel,
        _ => unreachable!(),
    };
    assert!(select_pushdown_predicate(bound1.filter.as_ref()).is_none());

    // 2. AND with NotEq and Eq must select Eq, not NotEq or AND as a whole
    let ast2 = parse_one("SELECT name FROM items WHERE price != 100 AND id = 5;").unwrap();
    let bound2 = match bind(&ast2, &cat_snap).unwrap() {
        htap_sql::BoundStatement::AnalyticSelect(sel) => sel,
        _ => unreachable!(),
    };
    let pred2 = select_pushdown_predicate(bound2.filter.as_ref());
    assert_eq!(
        pred2,
        Some(htap_convert::Predicate::Eq {
            column: 0,
            value: Value::Int64(5),
        })
    );

    // 3. Range and null predicates are selected deterministically
    let ast3 = parse_one("SELECT name FROM items WHERE price >= 50;").unwrap();
    let bound3 = match bind(&ast3, &cat_snap).unwrap() {
        htap_sql::BoundStatement::AnalyticSelect(sel) => sel,
        _ => unreachable!(),
    };
    let pred3 = select_pushdown_predicate(bound3.filter.as_ref());
    assert_eq!(
        pred3,
        Some(htap_convert::Predicate::Gte {
            column: 2,
            value: Value::Int64(50),
        })
    );

    let ast4 = parse_one("SELECT name FROM items WHERE name IS NULL;").unwrap();
    let bound4 = match bind(&ast4, &cat_snap).unwrap() {
        htap_sql::BoundStatement::AnalyticSelect(sel) => sel,
        _ => unreachable!(),
    };
    let pred4 = select_pushdown_predicate(bound4.filter.as_ref());
    assert_eq!(pred4, Some(htap_convert::Predicate::IsNull { column: 1 }));

    // 4. Plan source columns planning and mapping
    let ast5 =
        parse_one("SELECT name, COUNT(*) FROM items WHERE price > 10 GROUP BY name;").unwrap();
    let bound5 = match bind(&ast5, &cat_snap).unwrap() {
        htap_sql::BoundStatement::AnalyticSelect(sel) => sel,
        _ => unreachable!(),
    };
    let (src_cols, mapping) = plan_source_columns(&bound5);
    // name is col 1, price is col 2
    assert_eq!(src_cols, vec![1, 2]);
    assert_eq!(mapping.get(&1), Some(&0));
    assert_eq!(mapping.get(&2), Some(&1));
}

#[test]
fn test_partitioned_native_range_topology_catalog_reopen_continuation() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();

        let schema = Schema::new(vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
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

        let def = PartitionedTableDefinition::new(
            "users",
            schema,
            vec![0],
            PartitionTopology::Range {
                key_column: 0,
                partitions: vec![
                    RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                    RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
                ],
            },
        );

        let res = server.create_partitioned_table(def.clone()).unwrap();
        assert_eq!(res, StatementResult::ddl(1));

        // Duplicate table rejection
        let err = server.create_partitioned_table(def).unwrap_err();
        assert!(matches!(err, HtapError::Conflict(_)));
    }

    // Inspect catalog on disk
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snap = cat_store.load().unwrap().expect("snapshot must exist");
    assert_eq!(snap.generation, 1);
    assert_eq!(snap.tables.len(), 1);
    assert_eq!(snap.partitions.len(), 2);
    assert_eq!(snap.tablets.len(), 2);
    assert_eq!(snap.replicas.len(), 2);

    let table = &snap.tables[0];
    assert_eq!(table.name, "users");
    assert_eq!(table.id.as_u64(), 1);
    assert_eq!(table.primary_key, vec![0]);
    assert_eq!(
        table.partitions,
        vec![PartitionId::new(1), PartitionId::new(2)]
    );
    assert_eq!(
        table.partitioning,
        Some(PartitioningDescriptor::new(0, PartitioningMethod::Range))
    );

    let p0 = snap.partition(PartitionId::new(1)).unwrap();
    assert_eq!(p0.name, "p0");
    assert_eq!(p0.table_id, TableId::new(1));
    assert_eq!(p0.storage, StorageDescriptor::Row);
    assert_eq!(p0.tablets, vec![TabletId::new(1)]);
    assert_eq!(
        p0.range,
        Some(RangeBound::new(Value::Int64(0), Value::Int64(100)))
    );

    let p1 = snap.partition(PartitionId::new(2)).unwrap();
    assert_eq!(p1.name, "p1");
    assert_eq!(p1.table_id, TableId::new(1));
    assert_eq!(p1.storage, StorageDescriptor::Row);
    assert_eq!(p1.tablets, vec![TabletId::new(2)]);
    assert_eq!(
        p1.range,
        Some(RangeBound::new(Value::Int64(100), Value::Int64(200)))
    );

    let t1 = snap.tablet(TabletId::new(1)).unwrap();
    assert_eq!(t1.partition_id, PartitionId::new(1));
    assert_eq!(t1.bucket, 0);
    assert_eq!(t1.replicas, vec![ReplicaId::new(1)]);

    let r1 = snap.replica(ReplicaId::new(1)).unwrap();
    assert_eq!(r1.tablet_id, TabletId::new(1));
    assert_eq!(r1.node_id, NodeId::new(1));
    assert!(r1.is_leader);
    assert!(r1.healthy);

    // Reopen server and verify persistence + ID continuation
    let server2 = LocalServer::open(dir.path()).unwrap();
    server2
        .execute("CREATE TABLE orders (id BIGINT PRIMARY KEY, amount DOUBLE);")
        .unwrap();

    let snap2 = cat_store.load().unwrap().expect("snapshot 2 must exist");
    assert_eq!(snap2.generation, 2);
    assert_eq!(snap2.tables.len(), 2);
    assert_eq!(snap2.partitions.len(), 3);
    assert_eq!(snap2.tablets.len(), 3);
    assert_eq!(snap2.replicas.len(), 3);

    let orders_tbl = snap2.table_by_name("orders").unwrap();
    assert_eq!(orders_tbl.id.as_u64(), 2);
    assert_eq!(orders_tbl.partitions[0].as_u64(), 3);

    let orders_part = snap2.partition(PartitionId::new(3)).unwrap();
    assert_eq!(orders_part.name, "p0");
    assert_eq!(orders_part.tablets[0].as_u64(), 3);

    let orders_tab = snap2.tablet(TabletId::new(3)).unwrap();
    assert_eq!(orders_tab.replicas[0].as_u64(), 3);
}

#[test]
fn test_partitioned_native_list_topology_catalog_reopen_continuation() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();

        let schema = Schema::new(vec![
            ColumnDef {
                name: "region".into(),
                data_type: DataType::String,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "revenue".into(),
                data_type: DataType::Float64,
                nullable: false,
                primary_key: false,
            },
        ])
        .unwrap();

        let def = PartitionedTableDefinition::new(
            "sales",
            schema,
            vec![0],
            PartitionTopology::List {
                key_column: 0,
                partitions: vec![
                    ListPartitionDefinition::new(
                        "us",
                        vec![
                            Value::String("US-EAST".into()),
                            Value::String("US-WEST".into()),
                        ],
                    ),
                    ListPartitionDefinition::new(
                        "eu",
                        vec![
                            Value::String("EU-CENTRAL".into()),
                            Value::String("EU-WEST".into()),
                        ],
                    ),
                ],
            },
        );

        let res = server.create_partitioned_table(def).unwrap();
        assert_eq!(res, StatementResult::ddl(1));
    }

    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let snap = cat_store.load().unwrap().expect("snapshot must exist");
    assert_eq!(snap.generation, 1);
    assert_eq!(snap.tables.len(), 1);
    assert_eq!(snap.partitions.len(), 2);

    let table = &snap.tables[0];
    assert_eq!(table.name, "sales");
    assert_eq!(
        table.partitioning,
        Some(PartitioningDescriptor::new(0, PartitioningMethod::List))
    );

    let p0 = snap.partition(PartitionId::new(1)).unwrap();
    assert_eq!(p0.name, "us");
    assert_eq!(
        p0.list_values,
        vec![
            Value::String("US-EAST".into()),
            Value::String("US-WEST".into()),
        ]
    );

    let p1 = snap.partition(PartitionId::new(2)).unwrap();
    assert_eq!(p1.name, "eu");
    assert_eq!(
        p1.list_values,
        vec![
            Value::String("EU-CENTRAL".into()),
            Value::String("EU-WEST".into()),
        ]
    );

    // Reopen server and add second partitioned table with 2 partitions
    let server2 = LocalServer::open(dir.path()).unwrap();
    let schema2 = Schema::new(vec![ColumnDef {
        name: "category".into(),
        data_type: DataType::String,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    let def2 = PartitionedTableDefinition::new(
        "products",
        schema2,
        vec![0],
        PartitionTopology::List {
            key_column: 0,
            partitions: vec![
                ListPartitionDefinition::new("electronics", vec![Value::String("PHONE".into())]),
                ListPartitionDefinition::new("apparel", vec![Value::String("SHIRT".into())]),
            ],
        },
    );
    server2.create_partitioned_table(def2).unwrap();

    let snap2 = cat_store.load().unwrap().expect("snapshot 2 must exist");
    assert_eq!(snap2.generation, 2);
    assert_eq!(snap2.tables.len(), 2);
    assert_eq!(snap2.partitions.len(), 4);
    assert_eq!(snap2.tablets.len(), 4);
    assert_eq!(snap2.replicas.len(), 4);

    let prod = snap2.table_by_name("products").unwrap();
    assert_eq!(prod.id.as_u64(), 2);
    assert_eq!(
        prod.partitions,
        vec![PartitionId::new(3), PartitionId::new(4)]
    );
}

#[test]
fn test_partitioned_boundary_unmatched_null_type_errors() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

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
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "t_range",
        schema.clone(),
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // 1. Boundary tests:
    // id = 0 (inclusive lower bound of p0)
    server
        .execute("INSERT INTO t_range (id, val) VALUES (0, 'zero');")
        .unwrap();
    // id = 99 (upper edge of p0)
    server
        .execute("INSERT INTO t_range (id, val) VALUES (99, 'ninety-nine');")
        .unwrap();
    // id = 100 (exclusive upper of p0, inclusive lower of p1)
    server
        .execute("INSERT INTO t_range (id, val) VALUES (100, 'hundred');")
        .unwrap();
    // id = 199 (upper edge of p1)
    server
        .execute("INSERT INTO t_range (id, val) VALUES (199, 'one-ninety-nine');")
        .unwrap();

    // Point reads on all boundary keys
    for &(k, expected) in &[
        (0, "zero"),
        (99, "ninety-nine"),
        (100, "hundred"),
        (199, "one-ninety-nine"),
    ] {
        let res = server
            .execute(&format!("SELECT val FROM t_range WHERE id = {k};"))
            .unwrap();
        match res {
            StatementResult::Query(qr) => {
                assert_eq!(qr.num_rows(), 1);
                assert_eq!(
                    qr.rows()[0].get(0),
                    Some(&Value::String(expected.to_string()))
                );
            }
            other => panic!("expected query result, got {other:?}"),
        }
    }

    // 2. Unmatched partition key tests
    // id = 200 (at upper bound of p1, which is exclusive -> unmatched!)
    let err_insert_200 = server
        .execute("INSERT INTO t_range (id, val) VALUES (200, 'two-hundred');")
        .unwrap_err();
    assert!(matches!(err_insert_200, HtapError::InvalidArgument(_)));
    assert!(err_insert_200
        .to_string()
        .contains("does not match any partition"));

    // id = -1 (below lower bound of p0 -> unmatched!)
    let err_insert_neg = server
        .execute("INSERT INTO t_range (id, val) VALUES (-1, 'neg');")
        .unwrap_err();
    assert!(matches!(err_insert_neg, HtapError::InvalidArgument(_)));
    assert!(err_insert_neg
        .to_string()
        .contains("does not match any partition"));

    // Point select on unmatched key
    let err_select_200 = server
        .execute("SELECT val FROM t_range WHERE id = 200;")
        .unwrap_err();
    assert!(matches!(err_select_200, HtapError::InvalidArgument(_)));
    assert!(err_select_200
        .to_string()
        .contains("does not match any partition"));

    // Delete on unmatched key
    let err_delete_200 = server
        .execute("DELETE FROM t_range WHERE id = 200;")
        .unwrap_err();
    assert!(matches!(err_delete_200, HtapError::InvalidArgument(_)));
    assert!(err_delete_200
        .to_string()
        .contains("does not match any partition"));

    // 3. Null partition key test
    let err_null = server
        .execute("INSERT INTO t_range (id, val) VALUES (NULL, 'null_pk');")
        .unwrap_err();
    assert!(matches!(err_null, HtapError::InvalidArgument(_)));

    // 4. Type mismatch tests
    // SQL insert with incompatible type for partition key
    let err_type = server
        .execute("INSERT INTO t_range (id, val) VALUES ('not_an_int', 'foo');")
        .unwrap_err();
    assert!(matches!(err_type, HtapError::InvalidArgument(_)));

    // create_partitioned_table with mismatched bound type
    let err_def_type = server
        .create_partitioned_table(PartitionedTableDefinition::new(
            "bad_type",
            schema.clone(),
            vec![0],
            PartitionTopology::Range {
                key_column: 0,
                partitions: vec![RangePartitionDefinition::new(
                    "p0",
                    Value::String("0".into()),
                    Value::String("100".into()),
                )],
            },
        ))
        .unwrap_err();
    assert!(matches!(err_def_type, HtapError::InvalidArgument(_)));

    // create_partitioned_table with lower >= upper
    let err_def_bounds = server
        .create_partitioned_table(PartitionedTableDefinition::new(
            "bad_bounds",
            schema.clone(),
            vec![0],
            PartitionTopology::Range {
                key_column: 0,
                partitions: vec![RangePartitionDefinition::new(
                    "p0",
                    Value::Int64(100),
                    Value::Int64(50),
                )],
            },
        ))
        .unwrap_err();
    assert!(matches!(err_def_bounds, HtapError::InvalidArgument(_)));

    // create_partitioned_table with overlapping ranges
    let err_def_overlap = server
        .create_partitioned_table(PartitionedTableDefinition::new(
            "bad_overlap",
            schema.clone(),
            vec![0],
            PartitionTopology::Range {
                key_column: 0,
                partitions: vec![
                    RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                    RangePartitionDefinition::new("p1", Value::Int64(50), Value::Int64(150)),
                ],
            },
        ))
        .unwrap_err();
    assert!(matches!(err_def_overlap, HtapError::InvalidArgument(_)));

    // create_partitioned_table for List with duplicate list values across partitions
    let err_def_dup_list = server
        .create_partitioned_table(PartitionedTableDefinition::new(
            "bad_dup_list",
            schema,
            vec![0],
            PartitionTopology::List {
                key_column: 0,
                partitions: vec![
                    ListPartitionDefinition::new("l0", vec![Value::Int64(1), Value::Int64(2)]),
                    ListPartitionDefinition::new("l1", vec![Value::Int64(2), Value::Int64(3)]),
                ],
            },
        ))
        .unwrap_err();
    assert!(matches!(err_def_dup_list, HtapError::InvalidArgument(_)));
}

#[test]
fn test_partitioned_multi_row_insert_spanning_partitions_one_version_point_delete() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
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

    let def = PartitionedTableDefinition::new(
        "users",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(10)),
                RangePartitionDefinition::new("p1", Value::Int64(10), Value::Int64(20)),
                RangePartitionDefinition::new("p2", Value::Int64(20), Value::Int64(30)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // Multi-row INSERT spanning all 3 partitions in a single statement
    let insert_res = server
        .execute(
            "INSERT INTO users (id, name) VALUES \
            (1, 'alice'), \
            (15, 'bob'), \
            (25, 'carol'), \
            (2, 'dave');",
        )
        .unwrap();

    // Must be 4 rows affected and exactly 1 version
    assert_eq!(insert_res, StatementResult::dml(4, Some(Version::new(2))));

    // Point select from each partition
    let sel1 = server
        .execute("SELECT name FROM users WHERE id = 1;")
        .unwrap();
    assert_eq!(
        sel1,
        StatementResult::query(
            vec![ColumnDef {
                name: "name".into(),
                data_type: DataType::String,
                nullable: true,
                primary_key: false
            }],
            vec![htap_common::types::Row::new(vec![Value::String(
                "alice".into()
            )])],
        )
    );

    let sel15 = server
        .execute("SELECT name FROM users WHERE id = 15;")
        .unwrap();
    assert_eq!(
        sel15,
        StatementResult::query(
            vec![ColumnDef {
                name: "name".into(),
                data_type: DataType::String,
                nullable: true,
                primary_key: false
            }],
            vec![htap_common::types::Row::new(vec![Value::String(
                "bob".into()
            )])],
        )
    );

    let sel25 = server
        .execute("SELECT name FROM users WHERE id = 25;")
        .unwrap();
    assert_eq!(
        sel25,
        StatementResult::query(
            vec![ColumnDef {
                name: "name".into(),
                data_type: DataType::String,
                nullable: true,
                primary_key: false
            }],
            vec![htap_common::types::Row::new(vec![Value::String(
                "carol".into()
            )])],
        )
    );

    // DELETE id = 15 (in p1)
    let del_res = server.execute("DELETE FROM users WHERE id = 15;").unwrap();
    assert_eq!(del_res, StatementResult::dml(1, Some(Version::new(3))));

    // Point select on id = 15 now returns empty
    let sel15_after = server
        .execute("SELECT name FROM users WHERE id = 15;")
        .unwrap();
    match sel15_after {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected query result, got {other:?}"),
    }

    // Other partitions unaffected
    let sel1_after = server
        .execute("SELECT name FROM users WHERE id = 1;")
        .unwrap();
    match sel1_after {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 1),
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_partitioned_composite_pk_partition_key_not_first() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "tenant_id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "region_code".into(),
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

    // Partition key is region_code (index 1), which is the SECOND column in PK (vec![0, 1])
    let def = PartitionedTableDefinition::new(
        "tenants",
        schema,
        vec![0, 1],
        PartitionTopology::Range {
            key_column: 1,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int32(1), Value::Int32(10)),
                RangePartitionDefinition::new("p1", Value::Int32(10), Value::Int32(20)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // Insert rows
    server
        .execute(
            "INSERT INTO tenants (tenant_id, region_code, name) VALUES \
            (100, 5, 'Corp A'), \
            (100, 15, 'Corp B'), \
            (200, 5, 'Corp C');",
        )
        .unwrap();

    // Point select with composite PK where partition key is not first
    let res_a = server
        .execute("SELECT name FROM tenants WHERE tenant_id = 100 AND region_code = 5;")
        .unwrap();
    match res_a {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Corp A".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let res_b = server
        .execute("SELECT name FROM tenants WHERE tenant_id = 100 AND region_code = 15;")
        .unwrap();
    match res_b {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Corp B".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // Delete composite PK
    server
        .execute("DELETE FROM tenants WHERE tenant_id = 100 AND region_code = 15;")
        .unwrap();

    let res_b_del = server
        .execute("SELECT name FROM tenants WHERE tenant_id = 100 AND region_code = 15;")
        .unwrap();
    match res_b_del {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected query result, got {other:?}"),
    }

    // Still present
    let res_c = server
        .execute("SELECT name FROM tenants WHERE tenant_id = 200 AND region_code = 5;")
        .unwrap();
    match res_c {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("Corp C".into())));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_partitioned_olap_across_partitions_and_empty_aggregate() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
        ColumnDef {
            name: "category".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "metrics",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // 1. Empty table aggregates across partitions
    let empty_global = server
        .execute("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM metrics;")
        .unwrap();
    match empty_global {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let r = &qr.rows()[0];
            assert_eq!(r.get(0), Some(&Value::Int64(0)));
            assert_eq!(r.get(1), Some(&Value::Null));
            assert_eq!(r.get(2), Some(&Value::Null));
            assert_eq!(r.get(3), Some(&Value::Null));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let empty_grouped = server
        .execute("SELECT category, COUNT(*) FROM metrics GROUP BY category;")
        .unwrap();
    match empty_grouped {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // 2. Insert data across both partitions
    server
        .execute(
            "INSERT INTO metrics (id, val, category) VALUES \
            (10, 100, 'A'), \
            (20, 200, 'B'), \
            (30, 300, 'A'), \
            (110, 400, 'A'), \
            (120, 500, 'B');",
        )
        .unwrap();

    // 3. Global aggregate across partitions
    let global_res = server
        .execute("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM metrics;")
        .unwrap();
    match global_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let r = &qr.rows()[0];
            assert_eq!(r.get(0), Some(&Value::Int64(5)));
            assert_eq!(r.get(1), Some(&Value::Int64(1500)));
            assert_eq!(r.get(2), Some(&Value::Int64(100)));
            assert_eq!(r.get(3), Some(&Value::Int64(500)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // 4. Grouped aggregate across partitions
    let grouped_res = server
        .execute("SELECT category, COUNT(*), SUM(val) FROM metrics GROUP BY category;")
        .unwrap();
    match grouped_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            let r0 = &qr.rows()[0];
            let r1 = &qr.rows()[1];
            assert_eq!(r0.get(0), Some(&Value::String("A".into())));
            assert_eq!(r0.get(1), Some(&Value::Int64(3)));
            assert_eq!(r0.get(2), Some(&Value::Int64(800)));

            assert_eq!(r1.get(0), Some(&Value::String("B".into())));
            assert_eq!(r1.get(1), Some(&Value::Int64(2)));
            assert_eq!(r1.get(2), Some(&Value::Int64(700)));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    // 5. Filter scan across partitions
    let filter_res = server
        .execute("SELECT id, val FROM metrics WHERE val >= 300;")
        .unwrap();
    match filter_res {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            let ids: Vec<i64> = qr
                .rows()
                .iter()
                .map(|r| match r.get(0).unwrap() {
                    Value::Int64(id) => *id,
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(ids, vec![30, 110, 120]);
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn test_convert_table_multi_partition_guard() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    let schema = Schema::new(vec![ColumnDef {
        name: "id".into(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "multi_part",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(50)),
                RangePartitionDefinition::new("p1", Value::Int64(50), Value::Int64(100)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // convert_table must fail with Unsupported because table has 2 partitions
    let err = server.convert_table("multi_part").unwrap_err();
    assert!(matches!(err, HtapError::Unsupported(_)));
    assert!(err
        .to_string()
        .contains("must have exactly one partition, found 2"));
}

#[test]
fn test_partitioned_empty_topology_rejection_no_catalog_mutation() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();

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
    .unwrap();

    // 1. Initially catalog is empty (no snapshot)
    assert!(cat_store.load().unwrap().is_none());

    // 2. Reject empty Range topology on uninitialized catalog
    let empty_range_def = PartitionedTableDefinition::new(
        "empty_range",
        schema.clone(),
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![],
        },
    );
    let err_range = server
        .create_partitioned_table(empty_range_def)
        .unwrap_err();
    assert!(
        matches!(err_range, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {err_range:?}"
    );
    assert!(err_range
        .to_string()
        .contains("must define at least one partition"));

    // Verify catalog still has no snapshot / no mutation
    assert!(cat_store.load().unwrap().is_none());

    // 3. Reject empty List topology on uninitialized catalog
    let empty_list_def = PartitionedTableDefinition::new(
        "empty_list",
        schema.clone(),
        vec![0],
        PartitionTopology::List {
            key_column: 0,
            partitions: vec![],
        },
    );
    let err_list = server.create_partitioned_table(empty_list_def).unwrap_err();
    assert!(
        matches!(err_list, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {err_list:?}"
    );
    assert!(err_list
        .to_string()
        .contains("must define at least one partition"));

    // Verify catalog still has no snapshot / no mutation
    assert!(cat_store.load().unwrap().is_none());

    // 4. Create a valid baseline table to advance catalog to generation 1
    server
        .execute("CREATE TABLE baseline (id BIGINT PRIMARY KEY, val VARCHAR);")
        .unwrap();

    let snap_before = cat_store.load().unwrap().expect("snapshot must exist");
    assert_eq!(snap_before.generation, 1);
    assert_eq!(snap_before.tables.len(), 1);
    assert_eq!(snap_before.partitions.len(), 1);
    assert_eq!(snap_before.tablets.len(), 1);
    assert_eq!(snap_before.replicas.len(), 1);

    // 5. Attempt empty Range table on populated catalog
    let err_range_pop = server
        .create_partitioned_table(PartitionedTableDefinition::new(
            "empty_range_2",
            schema.clone(),
            vec![0],
            PartitionTopology::Range {
                key_column: 0,
                partitions: vec![],
            },
        ))
        .unwrap_err();
    assert!(matches!(err_range_pop, HtapError::InvalidArgument(_)));
    assert!(err_range_pop
        .to_string()
        .contains("must define at least one partition"));

    // Verify catalog has not mutated at all
    let snap_after_range = cat_store.load().unwrap().expect("snapshot must exist");
    assert_eq!(snap_before, snap_after_range);

    // 6. Attempt empty List table on populated catalog
    let err_list_pop = server
        .create_partitioned_table(PartitionedTableDefinition::new(
            "empty_list_2",
            schema.clone(),
            vec![0],
            PartitionTopology::List {
                key_column: 0,
                partitions: vec![],
            },
        ))
        .unwrap_err();
    assert!(matches!(err_list_pop, HtapError::InvalidArgument(_)));
    assert!(err_list_pop
        .to_string()
        .contains("must define at least one partition"));

    // Verify catalog has not mutated at all
    let snap_after_list = cat_store.load().unwrap().expect("snapshot must exist");
    assert_eq!(snap_before, snap_after_list);

    // 7. Verify subsequent valid table gets expected sequential IDs (no IDs burned/leaked)
    let valid_range_def = PartitionedTableDefinition::new(
        "valid_range",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![RangePartitionDefinition::new(
                "p0",
                Value::Int64(0),
                Value::Int64(10),
            )],
        },
    );
    server.create_partitioned_table(valid_range_def).unwrap();

    let snap_final = cat_store
        .load()
        .unwrap()
        .expect("final snapshot must exist");
    assert_eq!(snap_final.generation, 2);
    assert_eq!(snap_final.tables.len(), 2);
    assert_eq!(snap_final.partitions.len(), 2);
    assert_eq!(snap_final.tablets.len(), 2);
    assert_eq!(snap_final.replicas.len(), 2);

    let tbl = snap_final.table_by_name("valid_range").unwrap();
    assert_eq!(tbl.id.as_u64(), 2);
    assert_eq!(tbl.partitions[0].as_u64(), 2);

    let part = snap_final.partition(tbl.partitions[0]).unwrap();
    assert_eq!(part.tablets[0].as_u64(), 2);

    let tab = snap_final.tablet(part.tablets[0]).unwrap();
    assert_eq!(tab.replicas[0].as_u64(), 2);
}
