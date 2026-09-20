use htap_catalog::{CatalogSnapshot, StorageDescriptor, StorageFormat, TableDescriptor, TableId};
use htap_common::encode_key;
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

    // 2b. Assert ALTER PARTITIONS route under Row/Column/Converting -> CatalogDdl
    let bound_alter = BoundStatement::AlterPartitions(htap_sql::AlterPartitions::new(
        "orders",
        htap_catalog::PartitionAlteration::drop(vec!["p1"]),
    ));
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_alter, storage).expect("ALTER under storage"),
            Route::CatalogDdl
        );
    }

    // 3. Assert INSERT routes to RowstoreWrite under Row, Column, Converting
    let insert_sql = "INSERT INTO orders (tenant_id, order_id, amount) VALUES (42, 1000, 99.5)";
    let parsed_insert = parse_one(insert_sql).expect("parse INSERT");
    let bound_insert = bind(&parsed_insert, &catalog).expect("bind INSERT");
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_insert, storage).expect("INSERT classification"),
            Route::RowstoreWrite
        );
    }

    // A DELETE with a complete primary key uses the rowstore point-delete route.
    let delete_sql = "DELETE FROM orders WHERE tenant_id = 42 AND order_id = 1000";
    let parsed_delete = parse_one(delete_sql).expect("parse DELETE");
    let bound_delete = bind(&parsed_delete, &catalog).expect("bind DELETE");
    let expected_delete_key =
        encode_key(&[Value::Int32(42), Value::Int64(1000)]).expect("encode DELETE key");
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_delete, storage).expect("DELETE classification"),
            Route::RowstoreDelete {
                key: Some(expected_delete_key.clone())
            }
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

    // 5. Analytical SELECT queries route to Route::OlapScan across all storage descriptors
    let scan_sql = "SELECT amount FROM orders";
    let parsed_scan = parse_one(scan_sql).expect("parse scan");
    let bound_scan = bind(&parsed_scan, &catalog).expect("bind scan");
    assert!(matches!(bound_scan, BoundStatement::AnalyticSelect(_)));
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_scan, storage).expect("scan route"),
            Route::OlapScan
        );
    }

    let partial_key_sql = "SELECT amount FROM orders WHERE tenant_id = 42";
    let parsed_partial = parse_one(partial_key_sql).expect("parse partial PK");
    let bound_partial = bind(&parsed_partial, &catalog).expect("bind partial PK");
    assert!(matches!(bound_partial, BoundStatement::AnalyticSelect(_)));
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_partial, storage).expect("partial PK route"),
            Route::OlapScan
        );
    }

    let aggregate_sql = "SELECT COUNT(*), SUM(amount) FROM orders";
    let parsed_aggregate = parse_one(aggregate_sql).expect("parse aggregate");
    let bound_aggregate = bind(&parsed_aggregate, &catalog).expect("bind aggregate");
    assert!(matches!(bound_aggregate, BoundStatement::AnalyticSelect(_)));
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_aggregate, storage).expect("aggregate route"),
            Route::OlapScan
        );
    }

    // 6. Non-point DELETEs bind successfully and use the general rowstore delete route.
    let full_delete_sql = "DELETE FROM orders";
    let bound_full_delete = bind(
        &parse_one(full_delete_sql).expect("parse full delete"),
        &catalog,
    )
    .expect("bind full delete");
    assert_eq!(
        classify_route(&bound_full_delete, &row_storage).expect("full delete route"),
        Route::RowstoreDelete { key: None }
    );

    let partial_delete_sql = "DELETE FROM orders WHERE tenant_id = 42";
    let bound_partial_delete = bind(
        &parse_one(partial_delete_sql).expect("parse partial delete"),
        &catalog,
    )
    .expect("bind partial delete");
    assert_eq!(
        classify_route(&bound_partial_delete, &row_storage).expect("partial delete route"),
        Route::RowstoreDelete { key: None }
    );

    let order_by_sql =
        "SELECT amount FROM orders WHERE tenant_id = 42 AND order_id = 1000 ORDER BY amount";
    let parsed_order_by = parse_one(order_by_sql).expect("parse ORDER BY");
    let bound_order_by = bind(&parsed_order_by, &catalog).expect("bind ORDER BY");
    assert_eq!(
        classify_route(&bound_order_by, &StorageDescriptor::Row).unwrap(),
        Route::OlapScan
    );

    // ORDER BY over an expression leaves the narrow slice and routes to the general executor.
    let general_sql =
        "SELECT amount FROM orders WHERE tenant_id = 42 AND order_id = 1000 ORDER BY amount + 1";
    let parsed_general = parse_one(general_sql).expect("parse ORDER BY expression");
    let bound_general = bind(&parsed_general, &catalog).expect("bind general query");
    assert!(matches!(bound_general, BoundStatement::Query(_)));
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_general, storage).unwrap(),
            Route::Query
        );
    }

    // UPDATE with a complete PK routes to the rowstore point update with the encoded key.
    let update_sql = "UPDATE orders SET amount = 100.0 WHERE tenant_id = 42 AND order_id = 1000";
    let parsed_update = parse_one(update_sql).expect("parse UPDATE");
    let bound_update = bind(&parsed_update, &catalog).expect("bind UPDATE");
    let expected_key = encode_key(&[Value::Int32(42), Value::Int64(1000)]).unwrap();
    for storage in [&row_storage, &col_storage, &conv_storage] {
        assert_eq!(
            classify_route(&bound_update, storage).unwrap(),
            Route::RowstoreUpdate {
                key: Some(expected_key.clone())
            }
        );
    }
    let scan_update_sql = "UPDATE orders SET amount = amount * 2 WHERE amount > 10";
    let bound_scan_update = bind(&parse_one(scan_update_sql).unwrap(), &catalog).unwrap();
    assert_eq!(
        classify_route(&bound_scan_update, &row_storage).unwrap(),
        Route::RowstoreUpdate { key: None }
    );

    let bound_drop = bind(&parse_one("DROP TABLE orders").unwrap(), &catalog).unwrap();
    assert_eq!(
        classify_route(&bound_drop, &row_storage).unwrap(),
        Route::CatalogDdl
    );
    let bound_show = bind(&parse_one("SHOW TABLES").unwrap(), &catalog).unwrap();
    assert_eq!(
        classify_route(&bound_show, &row_storage).unwrap(),
        Route::CatalogRead
    );
}

/// A `@`/`@@` variable anywhere in an otherwise-narrow `SELECT` forces the general query path:
/// the narrow point/analytic binders have no notion of `Expr::Variable` and would otherwise try
/// (and fail) to resolve `@x` as a column.
#[test]
fn test_bind_account_management_route_classification() {
    use htap_catalog::PrivilegeSet;
    use htap_sql::{
        AlterUserStatement, CreateUserStatement, DropUserStatement, GrantScope, GrantStatement,
        RevokeStatement, ShowGrantsStatement,
    };

    let statements = [
        BoundStatement::CreateUser(CreateUserStatement {
            username: "u".into(),
            if_not_exists: false,
            password: Some("p".into()),
        }),
        BoundStatement::AlterUser(AlterUserStatement {
            username: "u".into(),
            password: "q".into(),
            if_exists: false,
        }),
        BoundStatement::DropUser(DropUserStatement {
            usernames: vec!["u".into()],
            if_exists: false,
        }),
        BoundStatement::GrantPrivileges(GrantStatement {
            privileges: PrivilegeSet::SELECT,
            scope: GrantScope::Global,
            grantee: "u".into(),
        }),
        BoundStatement::RevokePrivileges(RevokeStatement {
            privileges: PrivilegeSet::SELECT,
            scope: GrantScope::Global,
            grantee: "u".into(),
        }),
        BoundStatement::ShowGrants(ShowGrantsStatement { for_username: None }),
    ];

    for statement in statements {
        assert_eq!(
            classify_route(&statement, &StorageDescriptor::Row).unwrap(),
            Route::CatalogDdl
        );
    }
}

#[test]
fn test_narrow_shape_gate_excludes_variables() {
    let ddl = "CREATE TABLE t (pk INT PRIMARY KEY, c INT)";
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

    // A variable in the projection: without the exclusion this would still look narrow
    // (`Expr::Identifier`) and bind to `PointSelect`, which cannot resolve `@x`.
    let bound_projection =
        bind(&parse_one("SELECT @x FROM t WHERE pk=1").unwrap(), &catalog).expect("bind SELECT @x");
    assert!(matches!(bound_projection, BoundStatement::Query(_)));
    assert_eq!(
        classify_route(&bound_projection, &row_storage).unwrap(),
        Route::Query
    );

    // A variable in the WHERE clause.
    let bound_filter = bind(&parse_one("SELECT c FROM t WHERE pk=@x").unwrap(), &catalog)
        .expect("bind SELECT ... WHERE pk=@x");
    assert!(matches!(bound_filter, BoundStatement::Query(_)));
    assert_eq!(
        classify_route(&bound_filter, &row_storage).unwrap(),
        Route::Query
    );
}

/// R5 pin: a complete-PK, simple-projection SELECT still binds to `PointSelect` and routes
/// to `RowstorePointRead`, never to the general query executor, even though the same table
/// participates in joins elsewhere. Adding any general clause (LIMIT, alias, join, OR)
/// leaves the point path instead of silently dropping the clause.
#[test]
fn test_point_read_fast_path_pinned_against_general_query_path() {
    let ddl = "CREATE TABLE orders (tenant_id INT, order_id BIGINT, amount DOUBLE, \
               PRIMARY KEY (tenant_id, order_id))";
    let bound_ddl = bind(&parse_one(ddl).unwrap(), &CatalogSnapshot::empty()).unwrap();
    let create = match bound_ddl {
        BoundStatement::CreateTable(c) => c,
        other => panic!("{other:?}"),
    };
    let table = TableDescriptor::new(
        TableId(1),
        create.name,
        create.schema,
        create.primary_key,
        vec![],
        1,
    );
    let catalog = CatalogSnapshot::new(1, vec![table], vec![], vec![], vec![]);
    let key = encode_key(&[Value::Int32(7), Value::Int64(8)]).unwrap();

    for sql in [
        "SELECT * FROM orders WHERE tenant_id = 7 AND order_id = 8",
        "SELECT amount FROM orders WHERE order_id = 8 AND tenant_id = 7",
        "SELECT amount, tenant_id FROM orders WHERE (tenant_id = 7) AND (order_id = 8)",
    ] {
        let bound = bind(&parse_one(sql).unwrap(), &catalog).unwrap();
        assert!(
            matches!(bound, BoundStatement::Select(_)),
            "{sql} must bind as PointSelect, got {bound:?}"
        );
        assert_eq!(
            classify_route(&bound, &StorageDescriptor::Column).unwrap(),
            Route::RowstorePointRead { key: key.clone() }
        );
    }

    for sql in [
        "SELECT * FROM orders WHERE tenant_id = 7 AND order_id = 8 LIMIT 1",
        "SELECT * FROM orders o WHERE tenant_id = 7 AND order_id = 8",
        "SELECT * FROM orders WHERE tenant_id = 7 AND order_id = 8 OR amount > 1",
        "SELECT * FROM orders a JOIN orders b ON a.tenant_id = b.tenant_id \
         WHERE a.tenant_id = 7 AND a.order_id = 8",
        "SELECT amount + 1 FROM orders WHERE tenant_id = 7 AND order_id = 8",
    ] {
        let bound = bind(&parse_one(sql).unwrap(), &catalog).unwrap();
        assert!(
            matches!(bound, BoundStatement::Query(_)),
            "{sql} must bind as a general Query, got {bound:?}"
        );
        assert_eq!(
            classify_route(&bound, &StorageDescriptor::Row).unwrap(),
            Route::Query
        );
    }
}
