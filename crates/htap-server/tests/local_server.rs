//! Comprehensive integration tests for `LocalServer`.

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{
    ConversionDescriptor, ConversionPhase, NodeId, PartitionId, PartitioningDescriptor,
    PartitioningMethod, RangeBound, ReplicaDescriptor, ReplicaId, StorageDescriptor, StorageFormat,
    TableId, TabletId,
};
use htap_common::types::{ColumnDef, DataType, Row, Schema, Value};
use htap_common::version::Version;
use htap_common::HtapError;
use htap_movement::{CopyOptions, DataFormat, MovementJobPhase, TabletCloneOptions};
use htap_server::{
    ConversionAction, ConversionErrorCategory, ConversionPolicy, ListPartitionDefinition,
    LocalServer, PartitionAlteration, PartitionTopology, PartitionedTableDefinition,
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
fn test_truncate_empties_table_and_reports_affected_rows() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    server
        .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO t (id, v) VALUES (1, 10), (2, 20), (3, 30), (4, 40);")
        .unwrap();

    let truncate = server.execute("TRUNCATE TABLE t").unwrap();
    assert!(matches!(
        truncate,
        StatementResult::Command(CommandResult::Dml { affected: 4, .. })
    ));

    match server.execute("SELECT * FROM t").unwrap() {
        StatementResult::Query(query) => assert!(query.is_empty()),
        other => panic!("expected query result, got {other:?}"),
    }

    let empty_truncate = server.execute("TRUNCATE TABLE t").unwrap();
    assert!(matches!(
        empty_truncate,
        StatementResult::Command(CommandResult::Dml { affected: 0, .. })
    ));
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

    // The general query path reads the same partitions and fails the same way.
    let err_general = server
        .execute("SELECT val FROM c_table WHERE id = 1 ORDER BY id + 1;")
        .unwrap_err();
    assert!(
        matches!(err_general, HtapError::InvalidArgument(_)),
        "{err_general}"
    );
    assert!(
        err_general.to_string().contains("has no column manifest"),
        "unexpected error text: {err_general}"
    );

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

    // With a valid manifest the general query path reads the converting partition too.
    let general2 = server
        .execute("SELECT val FROM c_table WHERE id = 2 ORDER BY id + 1;")
        .unwrap();
    assert!(matches!(general2, StatementResult::Query(_)));
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
    server
        .execute("INSERT INTO users (id, age) VALUES (1, 30), (2, 10), (3, NULL);")
        .unwrap();

    let rows = |sql: &str| match server.execute(sql).unwrap() {
        StatementResult::Query(q) => q.rows,
        other => panic!("expected query result, got {other:?}"),
    };

    // Clauses outside the narrow analytic slice run through the general query executor.
    assert_eq!(
        rows("SELECT id FROM users ORDER BY age + 1 DESC;"),
        vec![
            Row::new(vec![Value::Int64(1)]),
            Row::new(vec![Value::Int64(2)]),
            Row::new(vec![Value::Int64(3)]),
        ]
    );
    assert_eq!(rows("SELECT id FROM users ORDER BY id LIMIT 2;").len(), 2);
    assert_eq!(
        rows("SELECT u1.id, u2.age FROM users u1 JOIN users u2 ON u1.id = u2.id WHERE u1.id = 2;"),
        vec![Row::new(vec![Value::Int64(2), Value::Int32(10)])]
    );
    assert_eq!(
        rows("SELECT AVG(age) FROM users;"),
        vec![Row::new(vec![Value::Float64(20.0)])]
    );
    assert_eq!(
        rows("SELECT age + 1 FROM users ORDER BY id;"),
        vec![
            Row::new(vec![Value::Int64(31)]),
            Row::new(vec![Value::Int64(11)]),
            Row::new(vec![Value::Null]),
        ]
    );

    // Window functions execute across the full result set and ignore NULL inputs.
    assert_eq!(
        rows("SELECT id, SUM(age) OVER () FROM users;"),
        vec![
            Row::new(vec![Value::Int64(1), Value::Int64(40)]),
            Row::new(vec![Value::Int64(2), Value::Int64(40)]),
            Row::new(vec![Value::Int64(3), Value::Int64(40)]),
        ]
    );
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

#[test]
fn test_partition_pruning_range_and_list_and_conservative_cases() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    // 1. Setup Range Partitioned Table
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

    let range_def = PartitionedTableDefinition::new(
        "items",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
                RangePartitionDefinition::new("p2", Value::Int64(200), Value::Int64(300)),
            ],
        },
    );
    server.create_partitioned_table(range_def).unwrap();

    server
        .execute(
            "INSERT INTO items (id, val, category) VALUES \
            (10, 100, 'A'), \
            (20, 200, 'B'), \
            (110, 300, 'A'), \
            (120, 400, 'B'), \
            (210, 500, 'A'), \
            (220, 600, 'B');",
        )
        .unwrap();

    // Range Pruning: Eq on partition key
    let q_eq = server
        .execute("SELECT id, val FROM items WHERE id = 110;")
        .unwrap();
    match q_eq {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(110)));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int64(300)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Range Pruning: Lt < 100 (only p0)
    let q_lt = server
        .execute("SELECT id, val FROM items WHERE id < 100 ORDER BY id;")
        .unwrap();
    match q_lt {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(10)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(20)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Range Pruning: Lte <= 110 (p0 and p1)
    let q_lte = server
        .execute("SELECT id, val FROM items WHERE id <= 110 ORDER BY id;")
        .unwrap();
    match q_lte {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(10)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(20)));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(110)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Range Pruning: Gt > 150 (only p2)
    let q_gt = server
        .execute("SELECT id, val FROM items WHERE id > 150 ORDER BY id;")
        .unwrap();
    match q_gt {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(210)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(220)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Range Pruning: AND conjunction Gte 100 AND Lt 200 (only p1)
    let q_and = server
        .execute("SELECT id, val FROM items WHERE id >= 100 AND id < 200 ORDER BY id;")
        .unwrap();
    match q_and {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(110)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(120)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Range Pruning: Provably empty range
    let q_empty1 = server
        .execute("SELECT id, val FROM items WHERE id < 0;")
        .unwrap();
    match q_empty1 {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected Query, got {other:?}"),
    }
    let q_empty2 = server
        .execute("SELECT id, val FROM items WHERE id >= 500;")
        .unwrap();
    match q_empty2 {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    // Conservative: != retains all partitions
    let q_ne = server
        .execute("SELECT id FROM items WHERE id != 10 ORDER BY id;")
        .unwrap();
    match q_ne {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 5);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(20)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(110)));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(120)));
            assert_eq!(qr.rows()[3].get(0), Some(&Value::Int64(210)));
            assert_eq!(qr.rows()[4].get(0), Some(&Value::Int64(220)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Conservative: non-partition key filter retains all partitions
    let q_non_pk = server
        .execute("SELECT id FROM items WHERE category = 'A' ORDER BY id;")
        .unwrap();
    match q_non_pk {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(10)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(110)));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(210)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Conservative: IsNull prunes all partitions
    let q_null = server
        .execute("SELECT id FROM items WHERE id IS NULL;")
        .unwrap();
    match q_null {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    // Conservative: IsNotNull retains all partitions
    let q_not_null = server
        .execute("SELECT COUNT(*) FROM items WHERE id IS NOT NULL;")
        .unwrap();
    match q_not_null {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(6)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 2. Setup List Partitioned Table
    let list_schema = Schema::new(vec![
        ColumnDef {
            name: "id".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "region".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "score".into(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let list_def = PartitionedTableDefinition::new(
        "regional",
        list_schema,
        vec![0, 1],
        PartitionTopology::List {
            key_column: 1,
            partitions: vec![
                ListPartitionDefinition::new(
                    "p_us",
                    vec![Value::String("US".into()), Value::String("CA".into())],
                ),
                ListPartitionDefinition::new(
                    "p_eu",
                    vec![Value::String("EU".into()), Value::String("UK".into())],
                ),
            ],
        },
    );
    server.create_partitioned_table(list_def).unwrap();

    server
        .execute(
            "INSERT INTO regional (id, region, score) VALUES \
            (1, 'US', 10), \
            (2, 'CA', 20), \
            (3, 'EU', 30), \
            (4, 'UK', 40);",
        )
        .unwrap();

    // List Pruning: Eq 'US' (only p_us)
    let q_us = server
        .execute("SELECT id, score FROM regional WHERE region = 'US';")
        .unwrap();
    match q_us {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(1)));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int64(10)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // List Pruning: Eq 'EU' (only p_eu)
    let q_eu = server
        .execute("SELECT id, score FROM regional WHERE region = 'EU';")
        .unwrap();
    match q_eu {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(3)));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int64(30)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // List Pruning: Eq 'JP' (unmatched, 0 partitions)
    let q_jp = server
        .execute("SELECT id, score FROM regional WHERE region = 'JP';")
        .unwrap();
    match q_jp {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    // Conservative: != 'US' (retains all partitions)
    let q_lne = server
        .execute("SELECT id, score FROM regional WHERE region != 'US' ORDER BY id;")
        .unwrap();
    match q_lne {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(2)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(3)));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::Int64(4)));
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_partition_storage_format_row_column_converting_equivalence() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();
    let cat_store =
        std::sync::Arc::new(LocalCatalogStore::open(dir.path().join("catalog")).unwrap());

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
            name: "note".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "hybrid",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
                RangePartitionDefinition::new("p2", Value::Int64(200), Value::Int64(300)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    server
        .execute(
            "INSERT INTO hybrid (id, val, note) VALUES \
            (10, 100, 'r1'), \
            (20, 200, 'r2'), \
            (110, 300, 'r3'), \
            (120, 400, 'r4'), \
            (210, 500, 'r5'), \
            (220, 600, 'r6');",
        )
        .unwrap();

    // Baseline queries against pure Rowstore
    let baseline_agg = server
        .execute("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM hybrid;")
        .unwrap();
    let baseline_scan = server
        .execute("SELECT id, val FROM hybrid ORDER BY id;")
        .unwrap();
    let baseline_filtered = server
        .execute("SELECT id, val FROM hybrid WHERE id >= 100 ORDER BY id;")
        .unwrap();

    // Now configure p1 as Column storage and p2 as Converting storage:
    let snap = cat_store.load().unwrap().unwrap();
    let p1_id = snap.partitions[1].id;
    let p2_id = snap.partitions[2].id;

    // Convert p1 using LocalConverter
    let converter = htap_convert::LocalConverter::new(
        std::sync::Arc::clone(&cat_store) as std::sync::Arc<dyn CatalogStore>,
        std::sync::Arc::new(
            htap_rowstore::Engine::open(htap_rowstore::EngineOptions::new(
                dir.path().join("rowstore"),
            ))
            .unwrap(),
        ),
        server.colstore_dir(),
        htap_convert::SegmentOptions::default(),
    );
    converter.convert_partition(p1_id).unwrap();

    // Modify p2 in catalog to Converting storage with SnapshotPinned phase
    let mut snap2 = cat_store.load().unwrap().unwrap();
    let conv_gen = snap2.generation + 1;
    snap2.generation = conv_gen;
    let p2_part = snap2.partitions.iter_mut().find(|p| p.id == p2_id).unwrap();
    p2_part.generation = conv_gen;
    p2_part.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: conv_gen,
    };
    p2_part.conversion = Some(ConversionDescriptor::new(
        conv_gen,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(2),
        ConversionPhase::SnapshotPinned,
    ));
    cat_store
        .compare_and_set(snap2.generation - 1, snap2)
        .unwrap();

    // Execute queries over mixed Row + Column + Converting partitions:
    let mixed_agg = server
        .execute("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM hybrid;")
        .unwrap();
    let mixed_scan = server
        .execute("SELECT id, val FROM hybrid ORDER BY id;")
        .unwrap();
    let mixed_filtered = server
        .execute("SELECT id, val FROM hybrid WHERE id >= 100 ORDER BY id;")
        .unwrap();

    // Verify exact equivalence between pure Rowstore and mixed formats!
    assert_eq!(mixed_agg, baseline_agg);
    assert_eq!(mixed_scan, baseline_scan);
    assert_eq!(mixed_filtered, baseline_filtered);
}

#[test]
fn test_multi_partition_global_aggregates_and_groups() {
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
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "dept".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "salaries",
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

    server
        .execute(
            "INSERT INTO salaries (id, val, dept) VALUES \
            (1, 100, 'eng'), \
            (2, 150, 'sales'), \
            (11, 200, 'eng'), \
            (12, NULL, 'sales'), \
            (21, 300, 'eng'), \
            (22, 250, 'hr');",
        )
        .unwrap();

    // 1. Global Aggregates across multiple partitions
    let agg = server
        .execute("SELECT COUNT(*), COUNT(val), SUM(val), MIN(val), MAX(val) FROM salaries;")
        .unwrap();
    match agg {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(6))); // COUNT(*)
            assert_eq!(row.get(1), Some(&Value::Int64(5))); // COUNT(val) excluding NULL
            assert_eq!(row.get(2), Some(&Value::Int64(1000))); // SUM(val)
            assert_eq!(row.get(3), Some(&Value::Int64(100))); // MIN(val)
            assert_eq!(row.get(4), Some(&Value::Int64(300))); // MAX(val)
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 2. Grouped Aggregates across multiple partitions with ORDER BY
    let grouped = server
        .execute("SELECT dept, COUNT(*), SUM(val) FROM salaries GROUP BY dept ORDER BY dept;")
        .unwrap();
    match grouped {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            // eng: 3 rows, sum 600
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("eng".into())));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::Int64(3)));
            assert_eq!(qr.rows()[0].get(2), Some(&Value::Int64(600)));
            // hr: 1 row, sum 250
            assert_eq!(qr.rows()[1].get(0), Some(&Value::String("hr".into())));
            assert_eq!(qr.rows()[1].get(1), Some(&Value::Int64(1)));
            assert_eq!(qr.rows()[1].get(2), Some(&Value::Int64(250)));
            // sales: 2 rows, sum 150
            assert_eq!(qr.rows()[2].get(0), Some(&Value::String("sales".into())));
            assert_eq!(qr.rows()[2].get(1), Some(&Value::Int64(2)));
            assert_eq!(qr.rows()[2].get(2), Some(&Value::Int64(150)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 3. Empty filter aggregate returns single row with 0/NULLs
    let empty_agg = server
        .execute("SELECT COUNT(*), SUM(val), MIN(val), MAX(val) FROM salaries WHERE val > 99999;")
        .unwrap();
    match empty_agg {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(0)));
            assert_eq!(row.get(1), Some(&Value::Null));
            assert_eq!(row.get(2), Some(&Value::Null));
            assert_eq!(row.get(3), Some(&Value::Null));
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_multi_partition_order_by_directions_nulls_and_tie_breaking() {
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
            name: "score".into(),
            data_type: DataType::Int32,
            nullable: true,
            primary_key: false,
        },
        ColumnDef {
            name: "team".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "leaderboard",
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

    // Insert across partitions with duplicates and NULLs:
    // p0: (1, NULL, 'blue'), (2, 20, 'red')
    // p1: (11, 10, 'green'), (12, 20, 'blue')
    // p2: (21, NULL, 'green'), (22, 30, 'red')
    server
        .execute(
            "INSERT INTO leaderboard (id, score, team) VALUES \
            (1, NULL, 'blue'), \
            (2, 20, 'red'), \
            (11, 10, 'green'), \
            (12, 20, 'blue'), \
            (21, NULL, 'green'), \
            (22, 30, 'red');",
        )
        .unwrap();

    // 1. ASC default: NULLS FIRST
    let q_asc = server
        .execute("SELECT id, score FROM leaderboard ORDER BY score ASC, id ASC;")
        .unwrap();
    match q_asc {
        StatementResult::Query(qr) => {
            let ids: Vec<i64> = qr
                .rows()
                .iter()
                .map(|r| match r.get(0).unwrap() {
                    Value::Int64(v) => *v,
                    _ => unreachable!(),
                })
                .collect();
            // NULLs first (ids 1, 21), then 10 (id 11), 20 (ids 2, 12), 30 (id 22)
            assert_eq!(ids, vec![1, 21, 11, 2, 12, 22]);
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 2. DESC default: NULLS LAST
    let q_desc = server
        .execute("SELECT id, score FROM leaderboard ORDER BY score DESC, id ASC;")
        .unwrap();
    match q_desc {
        StatementResult::Query(qr) => {
            let ids: Vec<i64> = qr
                .rows()
                .iter()
                .map(|r| match r.get(0).unwrap() {
                    Value::Int64(v) => *v,
                    _ => unreachable!(),
                })
                .collect();
            // 30 (id 22), 20 (ids 2, 12), 10 (id 11), then NULLs last (ids 1, 21)
            assert_eq!(ids, vec![22, 2, 12, 11, 1, 21]);
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 3. Explicit ASC NULLS LAST
    let q_asc_nl = server
        .execute("SELECT id, score FROM leaderboard ORDER BY score ASC NULLS LAST, id ASC;")
        .unwrap();
    match q_asc_nl {
        StatementResult::Query(qr) => {
            let ids: Vec<i64> = qr
                .rows()
                .iter()
                .map(|r| match r.get(0).unwrap() {
                    Value::Int64(v) => *v,
                    _ => unreachable!(),
                })
                .collect();
            // 10 (id 11), 20 (ids 2, 12), 30 (id 22), then NULLs (ids 1, 21)
            assert_eq!(ids, vec![11, 2, 12, 22, 1, 21]);
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 4. Explicit DESC NULLS FIRST
    let q_desc_nf = server
        .execute("SELECT id, score FROM leaderboard ORDER BY score DESC NULLS FIRST, id ASC;")
        .unwrap();
    match q_desc_nf {
        StatementResult::Query(qr) => {
            let ids: Vec<i64> = qr
                .rows()
                .iter()
                .map(|r| match r.get(0).unwrap() {
                    Value::Int64(v) => *v,
                    _ => unreachable!(),
                })
                .collect();
            // NULLs first (ids 1, 21), then 30 (id 22), 20 (ids 2, 12), 10 (id 11)
            assert_eq!(ids, vec![1, 21, 22, 2, 12, 11]);
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 5. Deterministic tie-breaking on full output row
    let q_tie = server
        .execute("SELECT score, team FROM leaderboard WHERE score = 20 ORDER BY score ASC;")
        .unwrap();
    match q_tie {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            // Tie-break on full output row: ('20', 'blue') < ('20', 'red')
            assert_eq!(qr.rows()[0].get(1), Some(&Value::String("blue".into())));
            assert_eq!(qr.rows()[1].get(1), Some(&Value::String("red".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 6. Grouped ORDER BY across partitions
    let q_grp = server
        .execute("SELECT team, COUNT(*) FROM leaderboard GROUP BY team ORDER BY team DESC;")
        .unwrap();
    match q_grp {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 3);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("red".into())));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::String("green".into())));
            assert_eq!(qr.rows()[2].get(0), Some(&Value::String("blue".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_scan_worker_count_equivalence() {
    let dir = TempDir::new().unwrap();
    let mut server = LocalServer::open(dir.path()).unwrap();

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
            name: "tag".into(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "parallel_data",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(10)),
                RangePartitionDefinition::new("p1", Value::Int64(10), Value::Int64(20)),
                RangePartitionDefinition::new("p2", Value::Int64(20), Value::Int64(30)),
                RangePartitionDefinition::new("p3", Value::Int64(30), Value::Int64(40)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    server
        .execute(
            "INSERT INTO parallel_data (id, val, tag) VALUES \
            (1, 100, 'X'), (2, 200, 'Y'), \
            (11, 150, 'X'), (12, 250, 'Z'), \
            (21, 300, 'Y'), (22, 100, 'Z'), \
            (31, 400, 'X'), (32, 350, 'Y');",
        )
        .unwrap();

    // Query 1: Filter + Projection + ORDER BY
    let q1 = "SELECT id, val, tag FROM parallel_data WHERE val >= 150 ORDER BY val DESC, id ASC;";
    // Query 2: Grouped Aggregation + ORDER BY
    let q2 = "SELECT tag, COUNT(*), SUM(val) FROM parallel_data GROUP BY tag ORDER BY tag ASC;";

    let mut q1_results = Vec::new();
    let mut q2_results = Vec::new();

    for &workers in &[1, 2, 4, 8] {
        server.set_scan_workers(workers);
        assert_eq!(server.scan_workers(), workers);

        let r1 = server.execute(q1).unwrap();
        let r2 = server.execute(q2).unwrap();

        q1_results.push(r1);
        q2_results.push(r2);
    }

    // All worker counts must produce bit-for-bit identical results
    for i in 1..q1_results.len() {
        assert_eq!(
            q1_results[0], q1_results[i],
            "mismatch for query 1 at worker index {i}"
        );
        assert_eq!(
            q2_results[0], q2_results[i],
            "mismatch for query 2 at worker index {i}"
        );
    }
}

#[test]
fn test_point_read_fast_path_unchanged() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    // 1. Unpartitioned table point reads
    server
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT);")
        .unwrap();
    server
        .execute("INSERT INTO users (id, name, age) VALUES (1, 'alice', 30), (2, 'bob', 25);")
        .unwrap();

    let p1 = server
        .execute("SELECT id, name, age FROM users WHERE id = 1;")
        .unwrap();
    match p1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(1)));
            assert_eq!(qr.rows()[0].get(1), Some(&Value::String("alice".into())));
            assert_eq!(qr.rows()[0].get(2), Some(&Value::Int32(30)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // Absent point read on unpartitioned table preserves column metadata with 0 rows
    let p_absent = server
        .execute("SELECT id, name, age FROM users WHERE id = 999;")
        .unwrap();
    match p_absent {
        StatementResult::Query(qr) => {
            assert!(qr.is_empty());
            assert_eq!(qr.num_rows(), 0);
            assert_eq!(qr.columns().len(), 3);
            assert_eq!(qr.columns()[0].name, "id");
            assert_eq!(qr.columns()[1].name, "name");
            assert_eq!(qr.columns()[2].name, "age");
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 2. Partitioned table point reads
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
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "p_table",
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

    server
        .execute("INSERT INTO p_table (id, val) VALUES (42, 'answer'), (142, 'more');")
        .unwrap();

    let p_part = server
        .execute("SELECT val FROM p_table WHERE id = 142;")
        .unwrap();
    match p_part {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("more".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let p_part_absent = server
        .execute("SELECT val FROM p_table WHERE id = 199;")
        .unwrap();
    match p_part_absent {
        StatementResult::Query(qr) => {
            assert!(qr.is_empty());
            assert_eq!(qr.num_rows(), 0);
            assert_eq!(qr.columns().len(), 1);
            assert_eq!(qr.columns()[0].name, "val");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_sql_range_partitioning_ddl_and_maxvalue_routing() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    // 1. Create partitioned table via SQL DDL with RANGE and MAXVALUE
    let ddl = "CREATE TABLE sales (id BIGINT PRIMARY KEY, amount DOUBLE) \
               PARTITION BY RANGE (id) ( \
                   PARTITION p0 VALUES LESS THAN (100), \
                   PARTITION p1 VALUES LESS THAN (200), \
                   PARTITION p_max VALUES LESS THAN MAXVALUE \
               );";
    let res = server.execute(ddl).unwrap();
    assert_eq!(res, StatementResult::ddl(1));

    // Verify catalog structure
    let cat_store = LocalCatalogStore::open(temp.path().join("catalog")).unwrap();
    let snap = cat_store.load().unwrap().unwrap();
    let tbl = snap.table_by_name("sales").unwrap();
    assert_eq!(tbl.partitions.len(), 3);
    let p_desc = tbl.partitioning.as_ref().unwrap();
    assert_eq!(p_desc.method, PartitioningMethod::Range);
    assert_eq!(p_desc.key_column, 0);

    // Verify RangeBound endpoints in catalog
    let p0 = snap
        .partitions
        .iter()
        .find(|p| p.table_id == tbl.id && p.name == "p0")
        .unwrap();
    assert_eq!(p0.range.as_ref().unwrap().lower, None);
    assert_eq!(p0.range.as_ref().unwrap().upper, Some(Value::Int64(100)));
    let p1 = snap
        .partitions
        .iter()
        .find(|p| p.table_id == tbl.id && p.name == "p1")
        .unwrap();
    assert_eq!(p1.range.as_ref().unwrap().lower, Some(Value::Int64(100)));
    assert_eq!(p1.range.as_ref().unwrap().upper, Some(Value::Int64(200)));
    let p_max = snap
        .partitions
        .iter()
        .find(|p| p.table_id == tbl.id && p.name == "p_max")
        .unwrap();
    assert_eq!(p_max.range.as_ref().unwrap().lower, Some(Value::Int64(200)));
    assert_eq!(p_max.range.as_ref().unwrap().upper, None);

    // 2. Multi-row insert across partitions in one commit version
    let insert_sql = "INSERT INTO sales (id, amount) VALUES (50, 10.5), (150, 20.5), (999, 99.9);";
    let ins_res = server.execute(insert_sql).unwrap();
    assert_eq!(ins_res, StatementResult::dml(3, Some(Version::new(2))));

    // 3. Point lookups
    let s0 = server
        .execute("SELECT amount FROM sales WHERE id = 50;")
        .unwrap();
    match s0 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(10.5)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let s_max = server
        .execute("SELECT amount FROM sales WHERE id = 999;")
        .unwrap();
    match s_max {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(99.9)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 4. Point delete
    let del_res = server.execute("DELETE FROM sales WHERE id = 150;").unwrap();
    assert_eq!(del_res, StatementResult::dml(1, Some(Version::new(3))));

    let s1_del = server
        .execute("SELECT amount FROM sales WHERE id = 150;")
        .unwrap();
    match s1_del {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 0);
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 5. Reopen server and verify state continuity
    drop(server);
    let reopened = LocalServer::open(temp.path()).unwrap();

    let s0_reopened = reopened
        .execute("SELECT amount FROM sales WHERE id = 50;")
        .unwrap();
    match s0_reopened {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(10.5)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let s_max_reopened = reopened
        .execute("SELECT amount FROM sales WHERE id = 999;")
        .unwrap();
    match s_max_reopened {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(99.9)));
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_sql_list_partitioning_ddl_and_routing() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    // 1. Create partitioned table via SQL DDL with LIST
    let ddl = "CREATE TABLE regions (code INT PRIMARY KEY, name VARCHAR) \
               PARTITION BY LIST (code) ( \
                   PARTITION p_us VALUES IN (1, 2), \
                   PARTITION p_eu VALUES IN (3, 4) \
               );";
    let res = server.execute(ddl).unwrap();
    assert_eq!(res, StatementResult::ddl(1));

    // 2. Insert across partitions in one commit version
    let ins_res = server
        .execute("INSERT INTO regions (code, name) VALUES (1, 'US-East'), (3, 'EU-Central');")
        .unwrap();
    assert_eq!(ins_res, StatementResult::dml(2, Some(Version::new(2))));

    // 3. Point lookups
    let q1 = server
        .execute("SELECT name FROM regions WHERE code = 1;")
        .unwrap();
    match q1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("US-East".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let q3 = server
        .execute("SELECT name FROM regions WHERE code = 3;")
        .unwrap();
    match q3 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("EU-Central".into()))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 4. Point delete
    let del = server
        .execute("DELETE FROM regions WHERE code = 1;")
        .unwrap();
    assert_eq!(del, StatementResult::dml(1, Some(Version::new(3))));

    // 5. Unmatched value insert rejected
    let err = server
        .execute("INSERT INTO regions (code, name) VALUES (99, 'Asia');")
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // 6. Reopen server and verify
    drop(server);
    let reopened = LocalServer::open(temp.path()).unwrap();
    let q3_reopened = reopened
        .execute("SELECT name FROM regions WHERE code = 3;")
        .unwrap();
    match q3_reopened {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("EU-Central".into()))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_server_alter_partitions_add_and_routing() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    // 1. Create range table with p0 [0, 100)
    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "balance".to_string(),
            data_type: DataType::Float64,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "accounts",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![RangePartitionDefinition::new(
                "p0",
                Value::Int64(0),
                Value::Int64(100),
            )],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // Insert row into p0
    server
        .execute("INSERT INTO accounts (id, balance) VALUES (50, 1000.0);")
        .unwrap();

    // 2. Alter partition: ADD p1 [100, 200)
    let add_res = server
        .alter_partitions(
            "accounts",
            PartitionAlteration::add(vec![RangePartitionDefinition::new(
                "p1",
                Value::Int64(100),
                Value::Int64(200),
            )]),
        )
        .unwrap();
    assert_eq!(add_res, StatementResult::ddl(1));

    // Insert row into p1
    server
        .execute("INSERT INTO accounts (id, balance) VALUES (150, 2000.0);")
        .unwrap();

    // Point queries for both
    let q50 = server
        .execute("SELECT balance FROM accounts WHERE id = 50;")
        .unwrap();
    match q50 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(1000.0)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let q150 = server
        .execute("SELECT balance FROM accounts WHERE id = 150;")
        .unwrap();
    match q150 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Float64(2000.0)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // OLAP query across both partitions
    let q_all = server
        .execute("SELECT id, balance FROM accounts WHERE id >= 0 ORDER BY id;")
        .unwrap();
    match q_all {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 2);
            assert_eq!(qr.rows()[0].get(0), Some(&Value::Int64(50)));
            assert_eq!(qr.rows()[1].get(0), Some(&Value::Int64(150)));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    // 3. List table ADD
    let list_schema = Schema::new(vec![
        ColumnDef {
            name: "code".to_string(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "name".to_string(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let list_def = PartitionedTableDefinition::new(
        "tenants",
        list_schema,
        vec![0],
        PartitionTopology::List {
            key_column: 0,
            partitions: vec![ListPartitionDefinition::new(
                "p_east",
                vec![Value::Int32(1), Value::Int32(2)],
            )],
        },
    );
    server.create_partitioned_table(list_def).unwrap();

    server
        .execute("INSERT INTO tenants (code, name) VALUES (1, 'Acme East');")
        .unwrap();

    server
        .alter_partitions(
            "tenants",
            PartitionAlteration::add(vec![ListPartitionDefinition::new(
                "p_west",
                vec![Value::Int32(3), Value::Int32(4)],
            )]),
        )
        .unwrap();

    server
        .execute("INSERT INTO tenants (code, name) VALUES (3, 'Acme West');")
        .unwrap();

    let q_list = server
        .execute("SELECT name FROM tenants WHERE code = 3;")
        .unwrap();
    match q_list {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("Acme West".into()))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_server_alter_partitions_drop_empty_and_populated_guard() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "val".to_string(),
            data_type: DataType::Float64,
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

    // Insert row into p1
    server
        .execute("INSERT INTO metrics (id, val) VALUES (120, 42.0);")
        .unwrap();

    // 1. Attempt to DROP populated partition p1 -> MUST FAIL
    let err = server
        .alter_partitions("metrics", PartitionAlteration::drop(vec!["p1"]))
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("populated"));

    // Verify catalog was NOT mutated
    let cat_store = LocalCatalogStore::open(temp.path().join("catalog")).unwrap();
    let catalog = cat_store.load().unwrap().unwrap();
    assert_eq!(catalog.generation, 1);
    let table = catalog.table_by_name("metrics").unwrap();
    assert_eq!(table.partitions.len(), 2);

    // Row is still queryable
    let q = server
        .execute("SELECT val FROM metrics WHERE id = 120;")
        .unwrap();
    match q {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 1),
        other => panic!("expected Query, got {other:?}"),
    }

    // 2. Insert into p0 and delete from p1
    server
        .execute("INSERT INTO metrics (id, val) VALUES (50, 10.0);")
        .unwrap();
    server
        .execute("DELETE FROM metrics WHERE id = 120;")
        .unwrap();

    // 3. Now p1 has tombstone, collapsed rowstore entries is empty -> DROP SUCCEEDS
    let drop_res = server
        .alter_partitions("metrics", PartitionAlteration::drop(vec!["p1"]))
        .unwrap();
    assert_eq!(drop_res, StatementResult::ddl(1));

    // Catalog mutated to generation 2
    let catalog2 = cat_store.load().unwrap().unwrap();
    assert_eq!(catalog2.generation, 2);
    let table2 = catalog2.table_by_name("metrics").unwrap();
    assert_eq!(table2.partitions.len(), 1);

    // Routing for 120 fails (no partition)
    let err_insert = server
        .execute("INSERT INTO metrics (id, val) VALUES (120, 99.0);")
        .unwrap_err();
    assert!(matches!(err_insert, HtapError::InvalidArgument(_)));

    // Row 50 in p0 still readable
    let q50 = server
        .execute("SELECT val FROM metrics WHERE id = 50;")
        .unwrap();
    match q50 {
        StatementResult::Query(qr) => assert_eq!(qr.num_rows(), 1),
        other => panic!("expected Query, got {other:?}"),
    }

    // 4. Attempt to drop last partition p0
    // First, when populated, fails with populated error
    let err_pop = server
        .alter_partitions("metrics", PartitionAlteration::drop(vec!["p0"]))
        .unwrap_err();
    assert!(matches!(err_pop, HtapError::InvalidArgument(_)));
    assert!(err_pop.to_string().contains("populated"));

    // Then delete row 50 so p0 is empty, and attempt again -> FAILS with no-last rule
    server
        .execute("DELETE FROM metrics WHERE id = 50;")
        .unwrap();
    let err_last = server
        .alter_partitions("metrics", PartitionAlteration::drop(vec!["p0"]))
        .unwrap_err();
    assert!(matches!(err_last, HtapError::InvalidArgument(_)));
    assert!(err_last.to_string().contains("cannot drop all partitions"));
}

#[test]
fn test_server_alter_partitions_reorganize_empty_and_populated_guard() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
        },
        ColumnDef {
            name: "msg".to_string(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "logs",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
                RangePartitionDefinition::new("p2", Value::Int64(200), Value::Int64(300)),
                RangePartitionDefinition::new("p3", Value::Int64(300), Value::Int64(400)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // Insert row into p2
    server
        .execute("INSERT INTO logs (id, msg) VALUES (250, 'important log');")
        .unwrap();

    // 1. Attempt reorganize on [p1, p2] where p2 is populated -> MUST FAIL
    let alt = PartitionAlteration::reorganize(
        vec!["p1", "p2"],
        vec![RangePartitionDefinition::new(
            "p12",
            Value::Int64(100),
            Value::Int64(300),
        )],
    );
    let err = server.alter_partitions("logs", alt.clone()).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("populated"));

    // Verify catalog not mutated
    let cat_store = LocalCatalogStore::open(temp.path().join("catalog")).unwrap();
    assert_eq!(cat_store.load().unwrap().unwrap().generation, 1);

    // 2. Delete row from p2 -> collapsed entries empty
    server.execute("DELETE FROM logs WHERE id = 250;").unwrap();

    // 3. Reorganize now SUCCEEDS
    let alt_new = PartitionAlteration::reorganize(
        vec!["p1", "p2"],
        vec![
            RangePartitionDefinition::new("p1_new", Value::Int64(100), Value::Int64(250)),
            RangePartitionDefinition::new("p2_new", Value::Int64(250), Value::Int64(300)),
        ],
    );
    let res = server.alter_partitions("logs", alt_new).unwrap();
    assert_eq!(res, StatementResult::ddl(1));

    // Verify catalog generation 2 and partition ordering preserved
    let cat = cat_store.load().unwrap().unwrap();
    assert_eq!(cat.generation, 2);
    let t = cat.table_by_name("logs").unwrap();
    let part_names: Vec<&str> = t
        .partitions
        .iter()
        .map(|&pid| cat.partition(pid).unwrap().name.as_str())
        .collect();
    assert_eq!(part_names, vec!["p0", "p1_new", "p2_new", "p3"]);

    // Insert into p1_new and p2_new
    server
        .execute("INSERT INTO logs (id, msg) VALUES (150, 'log in p1_new');")
        .unwrap();
    server
        .execute("INSERT INTO logs (id, msg) VALUES (275, 'log in p2_new');")
        .unwrap();

    let q150 = server
        .execute("SELECT msg FROM logs WHERE id = 150;")
        .unwrap();
    match q150 {
        StatementResult::Query(qr) => {
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("log in p1_new".into()))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let q275 = server
        .execute("SELECT msg FROM logs WHERE id = 275;")
        .unwrap();
    match q275 {
        StatementResult::Query(qr) => {
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("log in p2_new".into()))
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_server_alter_partitions_contiguous_and_negative_rules() {
    let temp = TempDir::new().unwrap();
    let server = LocalServer::open(temp.path()).unwrap();

    let schema = Schema::new(vec![ColumnDef {
        name: "id".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
    }])
    .unwrap();

    let def = PartitionedTableDefinition::new(
        "p_table",
        schema,
        vec![0],
        PartitionTopology::Range {
            key_column: 0,
            partitions: vec![
                RangePartitionDefinition::new("p0", Value::Int64(0), Value::Int64(100)),
                RangePartitionDefinition::new("p1", Value::Int64(100), Value::Int64(200)),
                RangePartitionDefinition::new("p2", Value::Int64(200), Value::Int64(300)),
            ],
        },
    );
    server.create_partitioned_table(def).unwrap();

    // 1. Non-contiguous reorganize: p0 and p2 (skipping p1)
    let alt_non_contig = PartitionAlteration::reorganize(
        vec!["p0", "p2"],
        vec![RangePartitionDefinition::new(
            "p02",
            Value::Int64(0),
            Value::Int64(100),
        )],
    );
    let err = server
        .alter_partitions("p_table", alt_non_contig)
        .unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
    assert!(err.to_string().contains("must be contiguous"));

    // 2. Unpartitioned table guard
    server
        .execute("CREATE TABLE unpart (id BIGINT PRIMARY KEY);")
        .unwrap();
    let err_unpart = server
        .alter_partitions("unpart", PartitionAlteration::drop(vec!["p0"]))
        .unwrap_err();
    assert!(matches!(err_unpart, HtapError::InvalidArgument(_)));
    assert!(err_unpart.to_string().contains("not partitioned"));

    // 3. Nonexistent table guard
    let err_ghost = server
        .alter_partitions("ghost_table", PartitionAlteration::drop(vec!["p0"]))
        .unwrap_err();
    assert!(matches!(err_ghost, HtapError::NotFound(_)));
}

#[test]
fn test_server_alter_partitions_reopen_continuation() {
    let temp = TempDir::new().unwrap();

    {
        let server = LocalServer::open(temp.path()).unwrap();

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
                nullable: false,
                primary_key: false,
            },
        ])
        .unwrap();

        let def = PartitionedTableDefinition::new(
            "events",
            schema,
            vec![0],
            PartitionTopology::Range {
                key_column: 0,
                partitions: vec![RangePartitionDefinition::new(
                    "p0",
                    Value::Int64(0),
                    Value::Int64(100),
                )],
            },
        );
        server.create_partitioned_table(def).unwrap();

        server
            .execute("INSERT INTO events (id, val) VALUES (42, 'event0');")
            .unwrap();

        // ADD p1
        server
            .alter_partitions(
                "events",
                PartitionAlteration::add(vec![RangePartitionDefinition::new(
                    "p1",
                    Value::Int64(100),
                    Value::Int64(200),
                )]),
            )
            .unwrap();

        server
            .execute("INSERT INTO events (id, val) VALUES (142, 'event1');")
            .unwrap();
    }

    // Reopen server from disk
    {
        let reopened = LocalServer::open(temp.path()).unwrap();

        let q0 = reopened
            .execute("SELECT val FROM events WHERE id = 42;")
            .unwrap();
        match q0 {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("event0".into())));
            }
            other => panic!("expected Query, got {other:?}"),
        }

        let q1 = reopened
            .execute("SELECT val FROM events WHERE id = 142;")
            .unwrap();
        match q1 {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("event1".into())));
            }
            other => panic!("expected Query, got {other:?}"),
        }

        // ADD p2 on reopened server
        reopened
            .alter_partitions(
                "events",
                PartitionAlteration::add(vec![RangePartitionDefinition::new(
                    "p2",
                    Value::Int64(200),
                    Value::Int64(300),
                )]),
            )
            .unwrap();

        reopened
            .execute("INSERT INTO events (id, val) VALUES (242, 'event2');")
            .unwrap();

        let q2 = reopened
            .execute("SELECT val FROM events WHERE id = 242;")
            .unwrap();
        match q2 {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("event2".into())));
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

#[test]
fn test_server_convert_table_multi_partition_reports_and_demotion_equivalence() {
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
        "events",
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

    server
        .execute("INSERT INTO events (id, val) VALUES (10, 'v10'), (20, 'v20'), (60, 'v60'), (70, 'v70');")
        .unwrap();

    // Baseline point read and OLAP queries
    let q_pt = server
        .execute("SELECT val FROM events WHERE id = 10;")
        .unwrap();
    let q_olap = server
        .execute("SELECT id, val FROM events ORDER BY id ASC;")
        .unwrap();

    // Convert multi-partition table to column
    let conv_report = server.convert_table_to_column("events").unwrap();
    assert_eq!(conv_report.table_name, "events");
    assert_eq!(conv_report.target_format, StorageFormat::Column);
    assert_eq!(conv_report.partitions.len(), 2);
    assert!(conv_report.is_success());
    assert_eq!(
        conv_report.partitions[0].action,
        ConversionAction::Converted
    );
    assert_eq!(
        conv_report.partitions[1].action,
        ConversionAction::Converted
    );

    // Queries on column storage match baseline
    let q_pt_col = server
        .execute("SELECT val FROM events WHERE id = 10;")
        .unwrap();
    let q_olap_col = server
        .execute("SELECT id, val FROM events ORDER BY id ASC;")
        .unwrap();
    assert_eq!(q_pt, q_pt_col);
    assert_eq!(q_olap, q_olap_col);

    // Demote table to row
    let demote_report = server.convert_table_to_row("events").unwrap();
    assert_eq!(demote_report.target_format, StorageFormat::Row);
    assert_eq!(demote_report.partitions.len(), 2);
    assert!(demote_report.is_success());
    assert_eq!(
        demote_report.partitions[0].action,
        ConversionAction::DemotedToRow
    );
    assert_eq!(
        demote_report.partitions[1].action,
        ConversionAction::DemotedToRow
    );
    assert_eq!(
        demote_report.partitions[0].final_storage,
        StorageDescriptor::Row
    );
    assert_eq!(
        demote_report.partitions[1].final_storage,
        StorageDescriptor::Row
    );

    // Verify catalog: tablet column_manifest cleared
    let cat = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let cat_snap = cat.load().unwrap().unwrap();
    for p in &cat_snap.partitions {
        assert_eq!(p.storage, StorageDescriptor::Row);
        assert!(p.conversion.is_none());
        let t = cat_snap.tablet(p.tablets[0]).unwrap();
        assert!(t.column_manifest.is_none());
    }

    // Queries after demotion match baseline (reverse demotion equivalence!)
    let q_pt_row = server
        .execute("SELECT val FROM events WHERE id = 10;")
        .unwrap();
    let q_olap_row = server
        .execute("SELECT id, val FROM events ORDER BY id ASC;")
        .unwrap();
    assert_eq!(q_pt, q_pt_row);
    assert_eq!(q_olap, q_olap_row);

    // Post-demotion writes and deletes
    server
        .execute("INSERT INTO events (id, val) VALUES (30, 'v30'), (80, 'v80');")
        .unwrap();
    server.execute("DELETE FROM events WHERE id = 10;").unwrap();

    let q_pt_deleted = server
        .execute("SELECT val FROM events WHERE id = 10;")
        .unwrap();
    match q_pt_deleted {
        StatementResult::Query(qr) => assert_eq!(qr.rows().len(), 0),
        other => panic!("expected Query, got {other:?}"),
    }

    let q_pt_new = server
        .execute("SELECT val FROM events WHERE id = 30;")
        .unwrap();
    match q_pt_new {
        StatementResult::Query(qr) => {
            assert_eq!(qr.rows()[0].get(0), Some(&Value::String("v30".into())));
        }
        other => panic!("expected Query, got {other:?}"),
    }

    let q_olap_post = server
        .execute("SELECT id FROM events ORDER BY id ASC;")
        .unwrap();
    match q_olap_post {
        StatementResult::Query(qr) => {
            let ids: Vec<i64> = qr
                .rows()
                .iter()
                .map(|r| match r.get(0).unwrap() {
                    Value::Int64(v) => *v,
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(ids, vec![20, 30, 60, 70, 80]);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn test_server_convert_mixed_success_and_blocking() {
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
        "t_mixed",
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

    server
        .execute("INSERT INTO t_mixed (id) VALUES (10), (60);")
        .unwrap();

    // Tamper with replica for p1 to make it non-leader
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let cur_cat = cat_store.load().unwrap().unwrap();
    let p1 = cur_cat.partitions.iter().find(|p| p.name == "p1").unwrap();
    let t1 = cur_cat.tablet(p1.tablets[0]).unwrap();
    let r1_id = t1.replicas[0];

    let next_gen = cur_cat.generation + 1;
    let mut modified_cat = cur_cat.clone();
    modified_cat.generation = next_gen;
    let r1_mut = modified_cat
        .replicas
        .iter_mut()
        .find(|r| r.id == r1_id)
        .unwrap();
    r1_mut.is_leader = false;
    cat_store
        .compare_and_set(cur_cat.generation, modified_cat)
        .unwrap();

    // Attempt table conversion to column: should partially succeed
    let rep = server.convert_table_to_column("t_mixed").unwrap();
    assert!(!rep.is_success());
    assert_eq!(rep.partitions.len(), 2);
    assert_eq!(rep.partitions[0].action, ConversionAction::Converted);
    assert_eq!(rep.partitions[0].final_storage, StorageDescriptor::Column);
    assert_eq!(rep.partitions[1].action, ConversionAction::Blocked);
    assert_eq!(rep.partitions[1].final_storage, StorageDescriptor::Row);
    assert_eq!(
        rep.partitions[1].error_category,
        Some(ConversionErrorCategory::Conflict)
    );
}

#[test]
fn test_server_conversion_tick_idempotent_and_resume_snapshot_pinned() {
    let dir = TempDir::new().unwrap();
    let server = LocalServer::open(dir.path()).unwrap();

    // Idempotent tick on empty server
    let tick1 = server.tick().unwrap();
    assert!(tick1.tables.is_empty());
    assert!(tick1.is_success());
    let tick2 = server.conversion_tick(ConversionPolicy::manual()).unwrap();
    assert!(tick2.tables.is_empty());

    // Create table with data
    server
        .execute("CREATE TABLE metrics (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    server
        .execute("INSERT INTO metrics (id, v) VALUES (1, 100);")
        .unwrap();

    // Put table partition in Converting with SnapshotPinned
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let cur_cat = cat_store.load().unwrap().unwrap();
    let p = &cur_cat.partitions[0];
    let part_id = p.id;
    let next_gen = cur_cat.generation + 1;
    let mut converting_cat = cur_cat.clone();
    converting_cat.generation = next_gen;
    let p_mut = converting_cat
        .partitions
        .iter_mut()
        .find(|p| p.id == part_id)
        .unwrap();
    p_mut.generation = next_gen;
    p_mut.storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: next_gen,
    };
    p_mut.conversion = Some(ConversionDescriptor::new(
        next_gen,
        StorageFormat::Row,
        StorageFormat::Column,
        Version::new(1),
        ConversionPhase::SnapshotPinned,
    ));
    cat_store
        .compare_and_set(cur_cat.generation, converting_cat)
        .unwrap();

    // Tick resumes the in-flight conversion
    let tick3 = server.tick().unwrap();
    assert_eq!(tick3.tables.len(), 1);
    let p_rep = tick3.tables[0].partition_report(part_id).unwrap();
    assert_eq!(p_rep.action, ConversionAction::Resumed);
    assert_eq!(p_rep.final_storage, StorageDescriptor::Column);
    assert!(p_rep.manifest.is_some());

    // Subsequent tick is idempotent
    let tick4 = server.tick().unwrap();
    assert!(tick4.tables.is_empty());
}

#[test]
fn test_server_open_fail_closed_missing_or_corrupt_manifest() {
    let dir = TempDir::new().unwrap();
    {
        let server = LocalServer::open(dir.path()).unwrap();
        server
            .execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v INT);")
            .unwrap();
        server
            .execute("INSERT INTO t (id, v) VALUES (1, 10);")
            .unwrap();
        server.convert_table_to_column("t").unwrap();
    }

    // Server closed cleanly.
    // Case 1: Corrupt manifest file
    let colstore = dir.path().join("colstore");
    let cat_store = LocalCatalogStore::open(dir.path().join("catalog")).unwrap();
    let cat = cat_store.load().unwrap().unwrap();
    let tablet_id = cat.partitions[0].tablets[0];
    let m_path = htap_convert::manifest_path(&colstore, tablet_id);
    assert!(m_path.is_file());

    std::fs::write(&m_path, b"CORRUPTED_GARBAGE_BYTES_MANIFEST").unwrap();

    // Reopen must fail closed with Corruption
    let err = LocalServer::open(dir.path()).unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "expected Corruption, got {err:?}"
    );

    // Case 2: Missing manifest file
    std::fs::remove_file(&m_path).unwrap();
    let err2 = LocalServer::open(dir.path()).unwrap_err();
    assert!(
        matches!(err2, HtapError::Io(_)) || matches!(err2, HtapError::Corruption(_)),
        "expected open failure, got {err2:?}"
    );
}

#[test]
fn test_server_sql_alter_partition_lifecycle() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().to_path_buf();

    {
        let server = LocalServer::open(&db_path).unwrap();

        // 1. Create RANGE partitioned table via SQL DDL
        server
            .execute(
                "CREATE TABLE events (id BIGINT PRIMARY KEY, val VARCHAR) \
                 PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (100), PARTITION p1 VALUES LESS THAN (200));",
            )
            .unwrap();

        // 2. ADD PARTITION via SQL
        server
            .execute("ALTER TABLE events ADD PARTITION (PARTITION p2 VALUES LESS THAN (300));")
            .unwrap();

        // Insert across p0, p1, p2
        server
            .execute("INSERT INTO events (id, val) VALUES (50, 'val_p0'), (150, 'val_p1'), (250, 'val_p2');")
            .unwrap();

        // Point queries verify data routing
        let q0 = server
            .execute("SELECT val FROM events WHERE id = 50;")
            .unwrap();
        match q0 {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("val_p0".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }
        let q2 = server
            .execute("SELECT val FROM events WHERE id = 250;")
            .unwrap();
        match q2 {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("val_p2".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }

        // 3. ADD PARTITION with MAXVALUE via SQL
        server
            .execute(
                "ALTER TABLE events ADD PARTITION (PARTITION p_max VALUES LESS THAN MAXVALUE);",
            )
            .unwrap();

        server
            .execute("INSERT INTO events (id, val) VALUES (999, 'val_max');")
            .unwrap();

        let q_max = server
            .execute("SELECT val FROM events WHERE id = 999;")
            .unwrap();
        match q_max {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("val_max".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }

        // 4. Adding partition after MAXVALUE must fail
        let err_after_max = server
            .execute(
                "ALTER TABLE events ADD PARTITION (PARTITION p_overflow VALUES LESS THAN (2000));",
            )
            .unwrap_err();
        assert!(
            matches!(err_after_max, HtapError::InvalidArgument(ref msg) if msg.contains("MAXVALUE partition already exists")),
            "expected MAXVALUE partition already exists error, got {err_after_max:?}"
        );

        // 5. DROP PARTITION: empty vs populated guard
        server
            .execute(
                "CREATE TABLE metrics (id BIGINT PRIMARY KEY, v INT) \
                 PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (20), PARTITION p2 VALUES LESS THAN (30));",
            )
            .unwrap();

        server
            .execute("INSERT INTO metrics (id, v) VALUES (5, 50), (15, 150);")
            .unwrap();

        // Attempt to drop populated p1 -> must fail closed with rowstore guard
        let err_drop_pop = server
            .execute("ALTER TABLE metrics DROP PARTITION p1;")
            .unwrap_err();
        assert!(
            matches!(err_drop_pop, HtapError::InvalidArgument(ref msg) if msg.contains("is populated with 1 row(s)")),
            "expected populated partition error, got {err_drop_pop:?}"
        );

        // Drop empty p2 -> must succeed
        server
            .execute("ALTER TABLE metrics DROP PARTITION p2;")
            .unwrap();

        // 6. REORGANIZE PARTITION: empty vs populated guard
        server
            .execute(
                "CREATE TABLE data (id BIGINT PRIMARY KEY, v INT) \
                 PARTITION BY RANGE (id) (PARTITION p0 VALUES LESS THAN (10), PARTITION p1 VALUES LESS THAN (20), PARTITION p2 VALUES LESS THAN (30));",
            )
            .unwrap();

        server
            .execute("INSERT INTO data (id, v) VALUES (5, 500);")
            .unwrap();

        // Attempt to reorganize populated p0, p1 -> must fail
        let err_reorg_pop = server
            .execute("ALTER TABLE data REORGANIZE PARTITION p0, p1 INTO (PARTITION p01 VALUES LESS THAN (20));")
            .unwrap_err();
        assert!(
            matches!(err_reorg_pop, HtapError::InvalidArgument(ref msg) if msg.contains("is populated with 1 row(s)")),
            "expected populated partition error on reorg, got {err_reorg_pop:?}"
        );

        // Reorganize empty contiguous p1, p2 -> must succeed
        server
            .execute(
                "ALTER TABLE data REORGANIZE PARTITION p1, p2 \
                 INTO (PARTITION p12a VALUES LESS THAN (25), PARTITION p12b VALUES LESS THAN (30));",
            )
            .unwrap();

        // Insert into reorganized partitions
        server
            .execute("INSERT INTO data (id, v) VALUES (22, 220), (28, 280);")
            .unwrap();
        let q_reorg = server.execute("SELECT v FROM data WHERE id = 22;").unwrap();
        match q_reorg {
            StatementResult::Query(qr) => assert_eq!(qr.rows()[0].get(0), Some(&Value::Int32(220))),
            other => panic!("expected Query, got {other:?}"),
        }

        // 7. LIST partitioning SQL ALTER
        server
            .execute(
                "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR) \
                 PARTITION BY LIST (id) (PARTITION p0 VALUES IN (1, 2), PARTITION p1 VALUES IN (3, 4));",
            )
            .unwrap();

        // ADD PARTITION LIST
        server
            .execute("ALTER TABLE items ADD PARTITION (PARTITION p2 VALUES IN (5, 6));")
            .unwrap();

        server
            .execute("INSERT INTO items (id, name) VALUES (5, 'item5');")
            .unwrap();
        let q_item = server
            .execute("SELECT name FROM items WHERE id = 5;")
            .unwrap();
        match q_item {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("item5".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }

        // DROP empty LIST partition p0
        server
            .execute("ALTER TABLE items DROP PARTITION p0;")
            .unwrap();
    }

    // 8. Reopen continuation and durability check
    {
        let reopened = LocalServer::open(&db_path).unwrap();

        // Events: check p0, p1, p2, p_max data still readable
        let q0 = reopened
            .execute("SELECT val FROM events WHERE id = 50;")
            .unwrap();
        match q0 {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("val_p0".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }
        let q_max = reopened
            .execute("SELECT val FROM events WHERE id = 999;")
            .unwrap();
        match q_max {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("val_max".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }

        // Data: check reorganized partition 22
        let q_reorg = reopened
            .execute("SELECT v FROM data WHERE id = 22;")
            .unwrap();
        match q_reorg {
            StatementResult::Query(qr) => assert_eq!(qr.rows()[0].get(0), Some(&Value::Int32(220))),
            other => panic!("expected Query, got {other:?}"),
        }

        // Items: check list partition item 5
        let q_item = reopened
            .execute("SELECT name FROM items WHERE id = 5;")
            .unwrap();
        match q_item {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows()[0].get(0), Some(&Value::String("item5".into())))
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
