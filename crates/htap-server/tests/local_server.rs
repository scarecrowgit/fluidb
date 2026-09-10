//! Comprehensive integration tests for `LocalServer`.

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_catalog::{ConversionDescriptor, ConversionPhase, StorageDescriptor, StorageFormat};
use htap_common::types::{DataType, Value};
use htap_common::version::Version;
use htap_common::HtapError;
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
