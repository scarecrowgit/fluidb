use htap_catalog::{CatalogSnapshot, StorageDescriptor, StorageFormat, TableDescriptor, TableId};
use htap_common::encode_key;
use htap_common::error::HtapError;
use htap_common::types::Value;
use htap_sql::{bind, classify_route, parse_one, BoundStatement, Route};

#[test]
fn test_route_classification() {
    // 1. Construct a minimal valid CatalogSnapshot/table through public parse_one + bind helpers.
    let ddl = "CREATE TABLE orders (\
        tenant_id INT, \
        order_id BIGINT, \
        amount DOUBLE, \
        PRIMARY KEY (tenant_id, order_id)\
    )";
    let parsed_ddl = parse_one(ddl).expect("parse CREATE TABLE");
    let bound_ddl = bind(&parsed_ddl, &CatalogSnapshot::empty()).expect("bind CREATE TABLE");
    let create_table = match &bound_ddl {
        BoundStatement::CreateTable(create) => create.clone(),
        other => panic!("expected CreateTable, got {other:?}"),
    };

    let table = TableDescriptor::new(
        TableId(1),
        create_table.name,
        create_table.schema,
        create_table.primary_key,
        vec![],
        1,
    );
    let catalog = CatalogSnapshot::new(1, vec![table], vec![], vec![], vec![]);

    let row_storage = StorageDescriptor::Row;
    let col_storage = StorageDescriptor::Column;
    let conv_storage = StorageDescriptor::Converting {
        from: StorageFormat::Row,
        to: StorageFormat::Column,
        generation: 1,
    };

    // 2. Assert CREATE route under Row/Column/Converting -> CatalogDdl
    assert_eq!(
        classify_route(&bound_ddl, &row_storage).expect("CREATE under Row"),
        Route::CatalogDdl
    );
    assert_eq!(
        classify_route(&bound_ddl, &col_storage).expect("CREATE under Column"),
        Route::CatalogDdl
    );
    assert_eq!(
        classify_route(&bound_ddl, &conv_storage).expect("CREATE under Converting"),
        Route::CatalogDdl
    );

    // 3. Assert INSERT / DELETE RowstoreWrite under Row, Column, Converting
    let insert_sql = "INSERT INTO orders (tenant_id, order_id, amount) VALUES (42, 1000, 99.5)";
    let parsed_insert = parse_one(insert_sql).expect("parse INSERT");
    let bound_insert = bind(&parsed_insert, &catalog).expect("bind INSERT");
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_insert, storage).expect("INSERT classification"),
            Route::RowstoreWrite
        );
    }

    let delete_sql = "DELETE FROM orders WHERE tenant_id = 42 AND order_id = 1000";
    let parsed_delete = parse_one(delete_sql).expect("parse DELETE");
    let bound_delete = bind(&parsed_delete, &catalog).expect("bind DELETE");
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_delete, storage).expect("DELETE classification"),
            Route::RowstoreWrite
        );
    }

    // 4. Composite-PK reverse predicate SELECT key exactly equals common encode_key in catalog PK order
    let select_sql = "SELECT amount FROM orders WHERE order_id = 1000 AND tenant_id = 42";
    let parsed_select = parse_one(select_sql).expect("parse SELECT");
    let bound_select = bind(&parsed_select, &catalog).expect("bind SELECT");

    // Catalog PK order is tenant_id (idx 0), order_id (idx 1)
    let expected_key_values = vec![Value::Int32(42), Value::Int64(1000)];
    let expected_encoded_key = encode_key(&expected_key_values).expect("encode_key");

    for storage in [&row_storage, &col_storage, &conv_storage] {
        let select_route = classify_route(&bound_select, storage).expect("SELECT classification");
        assert_eq!(
            select_route,
            Route::RowstorePointRead {
                key: expected_encoded_key.clone()
            }
        );
    }

    // 5. Non-point queries and unsupported operations remain rejected
    let scan_sql = "SELECT amount FROM orders";
    let parsed_scan = parse_one(scan_sql).expect("parse scan");
    let scan_err = bind(&parsed_scan, &catalog).expect_err("scan should fail binding");
    assert!(
        matches!(scan_err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for full table scan without WHERE, got {scan_err:?}"
    );

    let partial_key_sql = "SELECT amount FROM orders WHERE tenant_id = 42";
    let parsed_partial = parse_one(partial_key_sql).expect("parse partial PK");
    let partial_err = bind(&parsed_partial, &catalog).expect_err("partial PK query should fail");
    assert!(
        matches!(partial_err, HtapError::InvalidArgument(_)),
        "expected InvalidArgument for partial PK, got {partial_err:?}"
    );

    let unsupported_sql =
        "SELECT amount FROM orders WHERE tenant_id = 42 AND order_id = 1000 ORDER BY amount";
    let parsed_unsupported = parse_one(unsupported_sql).expect("parse ORDER BY");
    let unsupported_err = bind(&parsed_unsupported, &catalog).expect_err("ORDER BY should fail");
    assert!(
        matches!(unsupported_err, HtapError::Unsupported(_)),
        "expected Unsupported for ORDER BY, got {unsupported_err:?}"
    );

    let update_sql = "UPDATE orders SET amount = 100.0 WHERE tenant_id = 42 AND order_id = 1000";
    let parsed_update = parse_one(update_sql).expect("parse UPDATE");
    let update_err = bind(&parsed_update, &catalog).expect_err("UPDATE should fail");
    assert!(
        matches!(update_err, HtapError::Unsupported(_)),
        "expected Unsupported for UPDATE, got {update_err:?}"
    );
}
