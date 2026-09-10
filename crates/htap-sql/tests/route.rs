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

    // 3. Assert INSERT / DELETE RowstoreWrite under Row
    let insert_sql = "INSERT INTO orders (tenant_id, order_id, amount) VALUES (42, 1000, 99.5)";
    let parsed_insert = parse_one(insert_sql).expect("parse INSERT");
    let bound_insert = bind(&parsed_insert, &catalog).expect("bind INSERT");
    assert_eq!(
        classify_route(&bound_insert, &row_storage).expect("INSERT under Row"),
        Route::RowstoreWrite
    );

    let delete_sql = "DELETE FROM orders WHERE tenant_id = 42 AND order_id = 1000";
    let parsed_delete = parse_one(delete_sql).expect("parse DELETE");
    let bound_delete = bind(&parsed_delete, &catalog).expect("bind DELETE");
    assert_eq!(
        classify_route(&bound_delete, &row_storage).expect("DELETE under Row"),
        Route::RowstoreWrite
    );

    // 4. Composite-PK reverse predicate SELECT key exactly equals common encode_key in catalog PK order
    let select_sql = "SELECT amount FROM orders WHERE order_id = 1000 AND tenant_id = 42";
    let parsed_select = parse_one(select_sql).expect("parse SELECT");
    let bound_select = bind(&parsed_select, &catalog).expect("bind SELECT");

    // Catalog PK order is tenant_id (idx 0), order_id (idx 1)
    let expected_key_values = vec![Value::Int32(42), Value::Int64(1000)];
    let expected_encoded_key = encode_key(&expected_key_values).expect("encode_key");

    let select_route = classify_route(&bound_select, &row_storage).expect("SELECT under Row");
    assert_eq!(
        select_route,
        Route::RowstorePointRead {
            key: expected_encoded_key
        }
    );

    // 5. Column/Converting reject data statements with Unsupported
    let data_statements = [
        ("INSERT", &bound_insert),
        ("DELETE", &bound_delete),
        ("SELECT", &bound_select),
    ];

    for (name, stmt) in &data_statements {
        let col_err = classify_route(stmt, &col_storage)
            .expect_err(&format!("{name} under Column should fail"));
        assert!(
            matches!(col_err, HtapError::Unsupported(_)),
            "expected Unsupported for {name} under Column, got {col_err:?}"
        );

        let conv_err = classify_route(stmt, &conv_storage)
            .expect_err(&format!("{name} under Converting should fail"));
        assert!(
            matches!(conv_err, HtapError::Unsupported(_)),
            "expected Unsupported for {name} under Converting, got {conv_err:?}"
        );
    }
}
