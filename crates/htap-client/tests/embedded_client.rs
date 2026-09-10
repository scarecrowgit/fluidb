//! Integration tests for [`EmbeddedClient`].

#![forbid(unsafe_code)]

use htap_client::{CommandResult, EmbeddedClient, QueryResult, StatementResult};
use htap_common::types::{DataType, Value};
use htap_common::version::Version;
use htap_common::HtapError;
use tempfile::TempDir;

#[test]
fn test_embedded_client_crud_lifecycle_and_metadata() {
    let dir = TempDir::new().unwrap();
    let client = EmbeddedClient::open(dir.path()).unwrap();

    // 1. CREATE TABLE (DDL)
    let ddl_res = client
        .execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, age INT);")
        .unwrap();
    match ddl_res {
        StatementResult::Command(cmd) => {
            assert_eq!(cmd, CommandResult::Ddl { affected: 1 });
            assert_eq!(cmd.affected(), 1);
            assert_eq!(cmd.version(), None);
        }
        StatementResult::Query(_) => panic!("expected Command result for DDL"),
    }

    // 2. INSERT single row (DML)
    let ins1 = client
        .execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30);")
        .unwrap();
    match ins1 {
        StatementResult::Command(cmd) => {
            assert_eq!(
                cmd,
                CommandResult::Dml {
                    affected: 1,
                    version: Some(Version::new(2)),
                }
            );
            assert_eq!(cmd.affected(), 1);
            assert_eq!(cmd.version(), Some(Version::new(2)));
        }
        StatementResult::Query(_) => panic!("expected Command result for INSERT"),
    }

    // 3. INSERT multiple rows (DML)
    let ins_multi = client
        .execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25), (3, 'Charlie', 35);")
        .unwrap();
    match ins_multi {
        StatementResult::Command(cmd) => {
            assert_eq!(
                cmd,
                CommandResult::Dml {
                    affected: 2,
                    version: Some(Version::new(3)),
                }
            );
            assert_eq!(cmd.affected(), 2);
            assert_eq!(cmd.version(), Some(Version::new(3)));
        }
        StatementResult::Query(_) => panic!("expected Command result for multi-row INSERT"),
    }

    // 4. SELECT point lookup (SELECT *)
    let sel_all = client.execute("SELECT * FROM users WHERE id = 1;").unwrap();
    match sel_all {
        StatementResult::Query(ref qr) => {
            let res: &QueryResult = qr;
            assert!(!res.is_empty());
            assert_eq!(res.num_rows(), 1);

            let cols = res.columns();
            assert_eq!(cols.len(), 3);
            assert_eq!(cols[0].name, "id");
            assert_eq!(cols[0].data_type, DataType::Int64);
            assert_eq!(cols[1].name, "name");
            assert_eq!(cols[1].data_type, DataType::String);
            assert_eq!(cols[2].name, "age");
            assert_eq!(cols[2].data_type, DataType::Int32);

            let row = &res.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int64(1)));
            assert_eq!(row.get(1), Some(&Value::String("Alice".to_string())));
            assert_eq!(row.get(2), Some(&Value::Int32(30)));
        }
        StatementResult::Command(_) => panic!("expected Query result for SELECT"),
    }

    // 5. SELECT projected subset with reordered columns
    let sel_proj = client
        .execute("SELECT age, name FROM users WHERE id = 2;")
        .unwrap();
    match sel_proj {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            let cols = qr.columns();
            assert_eq!(cols.len(), 2);
            assert_eq!(cols[0].name, "age");
            assert_eq!(cols[0].data_type, DataType::Int32);
            assert_eq!(cols[1].name, "name");
            assert_eq!(cols[1].data_type, DataType::String);

            let row = &qr.rows()[0];
            assert_eq!(row.get(0), Some(&Value::Int32(25)));
            assert_eq!(row.get(1), Some(&Value::String("Bob".to_string())));
        }
        StatementResult::Command(_) => panic!("expected Query result for SELECT"),
    }

    // 6. SELECT non-existent key (returns empty QueryResult with column metadata preserved)
    let sel_missing = client
        .execute("SELECT name, age FROM users WHERE id = 9999;")
        .unwrap();
    match sel_missing {
        StatementResult::Query(qr) => {
            assert!(qr.is_empty());
            assert_eq!(qr.num_rows(), 0);
            assert_eq!(qr.columns().len(), 2);
            assert_eq!(qr.columns()[0].name, "name");
            assert_eq!(qr.columns()[1].name, "age");
            assert!(qr.rows().is_empty());
        }
        StatementResult::Command(_) => panic!("expected Query result for SELECT"),
    }

    // 7. DELETE point row
    let del = client.execute("DELETE FROM users WHERE id = 1;").unwrap();
    match del {
        StatementResult::Command(cmd) => {
            assert_eq!(
                cmd,
                CommandResult::Dml {
                    affected: 1,
                    version: Some(Version::new(4)),
                }
            );
            assert_eq!(cmd.affected(), 1);
            assert_eq!(cmd.version(), Some(Version::new(4)));
        }
        StatementResult::Query(_) => panic!("expected Command result for DELETE"),
    }

    // 8. SELECT deleted row must return empty
    let sel_del = client.execute("SELECT * FROM users WHERE id = 1;").unwrap();
    match sel_del {
        StatementResult::Query(qr) => {
            assert!(qr.is_empty());
            assert_eq!(qr.num_rows(), 0);
        }
        StatementResult::Command(_) => panic!("expected Query result"),
    }

    // 9. Other rows remain unaffected
    let sel_remain = client
        .execute("SELECT name FROM users WHERE id = 3;")
        .unwrap();
    match sel_remain {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("Charlie".to_string()))
            );
        }
        StatementResult::Command(_) => panic!("expected Query result"),
    }
}

#[test]
fn test_embedded_client_drop_reopen_and_version_continuity() {
    let dir = TempDir::new().unwrap();

    {
        let client = EmbeddedClient::open(dir.path()).unwrap();
        client
            .execute("CREATE TABLE items (id BIGINT PRIMARY KEY, sku VARCHAR);")
            .unwrap();

        let ins1 = client
            .execute("INSERT INTO items (id, sku) VALUES (1, 'item-alpha');")
            .unwrap();
        assert_eq!(ins1, StatementResult::dml(1, Some(Version::new(2))));

        let ins2 = client
            .execute("INSERT INTO items (id, sku) VALUES (2, 'item-beta');")
            .unwrap();
        assert_eq!(ins2, StatementResult::dml(1, Some(Version::new(3))));
    }

    // Drop and reopen client
    let reopened = EmbeddedClient::open(dir.path()).unwrap();

    // Verify existing rows survive drop/reopen
    let sel1 = reopened
        .execute("SELECT sku FROM items WHERE id = 1;")
        .unwrap();
    match sel1 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("item-alpha".to_string()))
            );
        }
        StatementResult::Command(_) => panic!("expected Query result"),
    }

    let sel2 = reopened
        .execute("SELECT sku FROM items WHERE id = 2;")
        .unwrap();
    match sel2 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("item-beta".to_string()))
            );
        }
        StatementResult::Command(_) => panic!("expected Query result"),
    }

    // Execute next DML statement: version must monotonically continue from 4
    let ins3 = reopened
        .execute("INSERT INTO items (id, sku) VALUES (3, 'item-gamma');")
        .unwrap();
    assert_eq!(ins3, StatementResult::dml(1, Some(Version::new(4))));

    let sel3 = reopened
        .execute("SELECT sku FROM items WHERE id = 3;")
        .unwrap();
    match sel3 {
        StatementResult::Query(qr) => {
            assert_eq!(qr.num_rows(), 1);
            assert_eq!(
                qr.rows()[0].get(0),
                Some(&Value::String("item-gamma".to_string()))
            );
        }
        StatementResult::Command(_) => panic!("expected Query result"),
    }

    // Execute DELETE: version must monotonically continue from 5
    let del2 = reopened.execute("DELETE FROM items WHERE id = 2;").unwrap();
    assert_eq!(del2, StatementResult::dml(1, Some(Version::new(5))));

    let sel_deleted = reopened
        .execute("SELECT sku FROM items WHERE id = 2;")
        .unwrap();
    match sel_deleted {
        StatementResult::Query(qr) => {
            assert!(qr.is_empty());
        }
        StatementResult::Command(_) => panic!("expected Query result"),
    }
}

#[test]
fn test_embedded_client_unsupported_sql_preserves_error_categories() {
    let dir = TempDir::new().unwrap();
    let client = EmbeddedClient::open(dir.path()).unwrap();

    client
        .execute("CREATE TABLE products (id BIGINT PRIMARY KEY, name VARCHAR, price INT);")
        .unwrap();

    // 1. Syntax parse error -> InvalidArgument
    let err_syntax = client.execute("NOT A VALID SQL STATEMENT;").unwrap_err();
    assert!(
        matches!(err_syntax, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for parse failure, got {err_syntax:?}"
    );

    // 2. Empty statement -> InvalidArgument
    let err_empty = client.execute("   ;  ").unwrap_err();
    assert!(
        matches!(err_empty, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for empty statement, got {err_empty:?}"
    );

    // 3. Duplicate table creation -> Conflict
    let err_conflict = client
        .execute("CREATE TABLE products (id BIGINT PRIMARY KEY, name VARCHAR, price INT);")
        .unwrap_err();
    assert!(
        matches!(err_conflict, HtapError::Conflict(_)),
        "expected Conflict for duplicate table, got {err_conflict:?}"
    );

    // 4. Missing/nonexistent table -> NotFound
    let err_not_found = client
        .execute("SELECT * FROM nonexistent_table WHERE id = 1;")
        .unwrap_err();
    assert!(
        matches!(err_not_found, HtapError::NotFound(_)),
        "expected NotFound for missing table, got {err_not_found:?}"
    );

    // 5. Full table scan without WHERE primary key -> InvalidArgument
    let err_scan = client.execute("SELECT * FROM products;").unwrap_err();
    assert!(
        matches!(err_scan, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for full scan without PK, got {err_scan:?}"
    );

    // 6. Unsupported statement type UPDATE -> Unsupported
    let err_update = client
        .execute("UPDATE products SET price = 99 WHERE id = 1;")
        .unwrap_err();
    assert!(
        matches!(err_update, HtapError::Unsupported(_)),
        "expected Unsupported for UPDATE, got {err_update:?}"
    );

    // 7. Unsupported query modifier ORDER BY -> Unsupported
    let err_order = client
        .execute("SELECT name FROM products WHERE id = 1 ORDER BY price;")
        .unwrap_err();
    assert!(
        matches!(err_order, HtapError::Unsupported(_)),
        "expected Unsupported for ORDER BY, got {err_order:?}"
    );

    // 8. Unsupported query modifier GROUP BY -> Unsupported
    let err_group = client
        .execute("SELECT name FROM products WHERE id = 1 GROUP BY name;")
        .unwrap_err();
    assert!(
        matches!(err_group, HtapError::Unsupported(_)),
        "expected Unsupported for GROUP BY, got {err_group:?}"
    );
}
