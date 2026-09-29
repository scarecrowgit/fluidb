use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use htap_tpcc::generate::{generate, Dataset};
use htap_tpcc::load::{load_dataset, LoadOptions, LoadReport};
use htap_tpcc::schema::{ddl_statements, TABLE_NAMES};
use tempfile::TempDir;

fn dataset_lengths(dataset: &Dataset) -> [usize; 9] {
    [
        dataset.warehouse.len(),
        dataset.district.len(),
        dataset.item.len(),
        dataset.stock.len(),
        dataset.customer.len(),
        dataset.history.len(),
        dataset.orders.len(),
        dataset.order_line.len(),
        dataset.new_order.len(),
    ]
}

#[test]
#[ignore = "loads full one-warehouse dataset (~600k rows); use --release mode and --ignored flag"]
fn load_with_warehouse_count_1_and_verify_counts_and_values() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let mut dataset = generate(1, 0).expect("generate one-warehouse TPC-C dataset");
    dataset.customer[0].c_discount = 1234;

    let LoadReport { reports } =
        load_dataset(&server, dir.path(), &dataset, &LoadOptions::default()).expect("load dataset");

    for ((table_name, report), expected) in TABLE_NAMES
        .iter()
        .zip(reports.iter())
        .zip(dataset_lengths(&dataset))
    {
        let expected = expected as u64;
        assert_eq!(report.records_read, expected, "{table_name} records read");
        assert_eq!(
            report.records_committed, expected,
            "{table_name} records committed"
        );
        assert_eq!(report.rows_written, expected, "{table_name} rows written");
        assert_eq!(report.records_skipped, 0, "{table_name} records skipped");
    }

    for (table_name, expected) in TABLE_NAMES.iter().zip(dataset_lengths(&dataset)) {
        let result = server
            .execute(&format!("SELECT COUNT(*) FROM {table_name}"))
            .expect("count loaded table rows");

        match result {
            StatementResult::Query(result) => {
                assert_eq!(result.num_rows(), 1, "{table_name} count result rows");
                assert_eq!(
                    result.rows()[0].get(0),
                    Some(&Value::Int64(expected as i64)),
                    "{table_name} loaded row count"
                );
            }
            other => panic!("expected query result for {table_name} count, got {other:?}"),
        }
    }

    let warehouse_result = server
        .execute("SELECT w_ytd, w_name FROM warehouse LIMIT 1")
        .expect("query sample warehouse");
    match warehouse_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            let row = &result.rows()[0];
            assert!(
                matches!(row.get(0), Some(&Value::Decimal { .. })),
                "expected decimal warehouse year-to-date value, got {:?}",
                row.get(0)
            );
            assert!(
                matches!(row.get(1), Some(&Value::String(_))),
                "expected string warehouse name, got {:?}",
                row.get(1)
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let customer_result = server
        .execute("SELECT c_discount FROM customer LIMIT 1")
        .expect("query four-decimal customer discount");
    match customer_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(
                result.rows()[0].get(0),
                Some(&Value::Decimal {
                    value: 1234,
                    precision: 4,
                    scale: 4,
                })
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let order_result = server
        .execute("SELECT o_entry_d FROM orders LIMIT 1")
        .expect("query sample order");
    match order_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert!(
                matches!(result.rows()[0].get(0), Some(&Value::Timestamp(_))),
                "expected timestamp order entry date, got {:?}",
                result.rows()[0].get(0)
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let order_line_result = server
        .execute(
            "SELECT ol_delivery_d FROM order_line \
             WHERE ol_delivery_d IS NULL LIMIT 1",
        )
        .expect("query undelivered order line");
    match order_line_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(result.rows()[0].get(0), Some(&Value::Null));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

fn small_consistent_subset(full: Dataset) -> Dataset {
    let mut subset = Dataset {
        warehouse: full.warehouse,
        district: full
            .district
            .into_iter()
            .filter(|row| row.d_w_id == 1 && row.d_id == 1)
            .collect(),
        item: full
            .item
            .into_iter()
            .filter(|row| (1..=200).contains(&row.i_id))
            .collect(),
        stock: full
            .stock
            .into_iter()
            .filter(|row| row.s_w_id == 1 && (1..=200).contains(&row.s_i_id))
            .collect(),
        customer: full
            .customer
            .into_iter()
            .filter(|row| row.c_w_id == 1 && row.c_d_id == 1 && (1..=30).contains(&row.c_id))
            .collect(),
        history: full
            .history
            .into_iter()
            .filter(|row| row.h_c_w_id == 1 && row.h_c_d_id == 1 && (1..=30).contains(&row.h_c_id))
            .collect(),
        orders: full
            .orders
            .into_iter()
            .filter(|row| row.o_w_id == 1 && row.o_d_id == 1 && (1..=30).contains(&row.o_c_id))
            .collect(),
        order_line: Vec::new(),
        new_order: Vec::new(),
    };

    let kept_order_ids: std::collections::HashSet<_> =
        subset.orders.iter().map(|row| row.o_id).collect();

    subset.order_line = full
        .order_line
        .into_iter()
        .filter(|row| row.ol_w_id == 1 && row.ol_d_id == 1 && kept_order_ids.contains(&row.ol_o_id))
        .map(|mut row| {
            row.ol_i_id = ((row.ol_i_id - 1) % 200) + 1;
            row
        })
        .collect();

    subset.new_order = full
        .new_order
        .into_iter()
        .filter(|row| row.no_w_id == 1 && row.no_d_id == 1 && kept_order_ids.contains(&row.no_o_id))
        .collect();

    subset
}

#[test]
fn load_small_subset_with_referential_consistency() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let mut full = generate(1, 0).expect("generate one-warehouse TPC-C dataset");
    full.customer
        .iter_mut()
        .find(|row| row.c_w_id == 1 && row.c_d_id == 1 && row.c_id == 1)
        .expect("customer 1 in district 1")
        .c_discount = 1234;

    let dataset = small_consistent_subset(full);

    assert!(
        dataset.orders.iter().any(|row| row.o_id < 2101),
        "subset must contain a delivered order"
    );
    assert!(
        dataset.orders.iter().any(|row| row.o_id >= 2101),
        "subset must contain an undelivered order"
    );

    assert!(
        dataset.district.iter().all(|district| {
            dataset
                .warehouse
                .iter()
                .any(|warehouse| warehouse.w_id == district.d_w_id)
        }),
        "every district must reference a loaded warehouse"
    );
    assert!(
        dataset.stock.iter().all(|stock| {
            dataset
                .warehouse
                .iter()
                .any(|warehouse| warehouse.w_id == stock.s_w_id)
                && dataset.item.iter().any(|item| item.i_id == stock.s_i_id)
        }),
        "every stock row must reference a loaded warehouse and item"
    );
    assert!(
        dataset.customer.iter().all(|customer| {
            dataset.district.iter().any(|district| {
                district.d_w_id == customer.c_w_id && district.d_id == customer.c_d_id
            })
        }),
        "every customer must reference a loaded district"
    );
    assert!(
        dataset.history.iter().all(|history| {
            dataset.customer.iter().any(|customer| {
                customer.c_w_id == history.h_c_w_id
                    && customer.c_d_id == history.h_c_d_id
                    && customer.c_id == history.h_c_id
            })
        }),
        "every history row must reference a loaded customer"
    );
    assert!(
        dataset.orders.iter().all(|order| {
            dataset.customer.iter().any(|customer| {
                customer.c_w_id == order.o_w_id
                    && customer.c_d_id == order.o_d_id
                    && customer.c_id == order.o_c_id
            })
        }),
        "every order must reference a loaded customer"
    );
    assert!(
        dataset.order_line.iter().all(|line| {
            dataset.orders.iter().any(|order| {
                order.o_w_id == line.ol_w_id
                    && order.o_d_id == line.ol_d_id
                    && order.o_id == line.ol_o_id
            }) && dataset
                .stock
                .iter()
                .any(|stock| stock.s_w_id == line.ol_supply_w_id && stock.s_i_id == line.ol_i_id)
        }),
        "every order line must reference a loaded order and stock row"
    );
    assert!(
        dataset.new_order.iter().all(|new_order| {
            dataset.orders.iter().any(|order| {
                order.o_w_id == new_order.no_w_id
                    && order.o_d_id == new_order.no_d_id
                    && order.o_id == new_order.no_o_id
            })
        }),
        "every new-order row must reference a loaded order"
    );
    assert!(
        dataset.orders.iter().all(|order| {
            order.o_ol_cnt as usize
                == dataset
                    .order_line
                    .iter()
                    .filter(|line| {
                        line.ol_w_id == order.o_w_id
                            && line.ol_d_id == order.o_d_id
                            && line.ol_o_id == order.o_id
                    })
                    .count()
        }),
        "every order's o_ol_cnt must equal its loaded order-line count"
    );

    let LoadReport { reports } =
        load_dataset(&server, dir.path(), &dataset, &LoadOptions::default()).expect("load subset");

    for ((table_name, report), expected) in TABLE_NAMES
        .iter()
        .zip(reports.iter())
        .zip(dataset_lengths(&dataset))
    {
        let expected = expected as u64;
        assert_eq!(report.records_read, expected, "{table_name} records read");
        assert_eq!(
            report.records_committed, expected,
            "{table_name} records committed"
        );
        assert_eq!(report.rows_written, expected, "{table_name} rows written");
        assert_eq!(report.records_skipped, 0, "{table_name} records skipped");
    }

    for (table_name, expected) in TABLE_NAMES.iter().zip(dataset_lengths(&dataset)) {
        let result = server
            .execute(&format!("SELECT COUNT(*) FROM {table_name}"))
            .expect("count loaded table rows");

        match result {
            StatementResult::Query(result) => {
                assert_eq!(result.num_rows(), 1, "{table_name} count result rows");
                assert_eq!(
                    result.rows()[0].get(0),
                    Some(&Value::Int64(expected as i64)),
                    "{table_name} loaded row count"
                );
            }
            other => panic!("expected query result for {table_name} count, got {other:?}"),
        }
    }

    let warehouse_result = server
        .execute("SELECT w_ytd, w_name FROM warehouse WHERE w_id = 1")
        .expect("query warehouse");
    match warehouse_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(
                result.rows()[0].get(0),
                Some(&Value::Decimal {
                    value: 30000000,
                    precision: 12,
                    scale: 2,
                }),
                "w_ytd must round-trip as DECIMAL(12,2)"
            );
            assert_eq!(
                result.rows()[0].get(1),
                Some(&Value::String(dataset.warehouse[0].w_name.clone())),
                "warehouse string value must round-trip exactly"
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let customer_result = server
        .execute(
            "SELECT c_discount FROM customer \
             WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1",
        )
        .expect("query customer discount");
    match customer_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(
                result.rows()[0].get(0),
                Some(&Value::Decimal {
                    value: 1234,
                    precision: 4,
                    scale: 4,
                }),
                "c_discount must round-trip as exactly 0.1234"
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let delivered_orders = server
        .execute(
            "SELECT o_id FROM orders \
             WHERE o_w_id = 1 AND o_d_id = 1 AND o_id < 2101 LIMIT 1",
        )
        .expect("query delivered order");
    match delivered_orders {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1, "expected a delivered order");
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let undelivered_orders = server
        .execute(
            "SELECT o_carrier_id FROM orders \
             WHERE o_w_id = 1 AND o_d_id = 1 AND o_id >= 2101 LIMIT 1",
        )
        .expect("query undelivered order");
    match undelivered_orders {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1, "expected an undelivered order");
            assert_eq!(result.rows()[0].get(0), Some(&Value::Null));
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let delivered_lines = server
        .execute(
            "SELECT ol_delivery_d FROM order_line \
             WHERE ol_w_id = 1 AND ol_d_id = 1 \
             AND ol_delivery_d IS NOT NULL LIMIT 1",
        )
        .expect("query delivered order line");
    match delivered_lines {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1, "expected a delivered order line");
            assert!(
                matches!(result.rows()[0].get(0), Some(&Value::Timestamp(_))),
                "expected non-NULL delivery timestamp, got {:?}",
                result.rows()[0].get(0)
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let undelivered_lines = server
        .execute(
            "SELECT ol_delivery_d FROM order_line \
             WHERE ol_w_id = 1 AND ol_d_id = 1 \
             AND ol_delivery_d IS NULL LIMIT 1",
        )
        .expect("query undelivered order line");
    match undelivered_lines {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1, "expected an undelivered order line");
            assert_eq!(result.rows()[0].get(0), Some(&Value::Null));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn reject_load_when_tables_already_exist() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let dataset = generate(1, 0).expect("generate one-warehouse TPC-C dataset");

    for statement in ddl_statements() {
        server.execute(&statement).expect("create TPC-C table");
    }

    for attempt in 1..=2 {
        let error = load_dataset(&server, dir.path(), &dataset, &LoadOptions::default())
            .expect_err("load into existing tables must be rejected");

        assert!(
            matches!(error, HtapError::InvalidArgument(_)),
            "attempt {attempt}: expected InvalidArgument, got {error:?}"
        );
        assert!(
            error.to_string().contains("already exists"),
            "attempt {attempt}: expected existing-table error, got {error}"
        );
    }
}

#[test]
fn reject_invalid_batch_size_before_ddl() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let dataset = generate(1, 0).expect("generate one-warehouse TPC-C dataset");

    let error = load_dataset(
        &server,
        dir.path(),
        &dataset,
        &LoadOptions { batch_rows: 0 },
    )
    .expect_err("zero batch rows must be rejected");

    assert!(
        matches!(error, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {error:?}"
    );
    assert!(
        error.to_string().contains("must be greater than 0"),
        "expected invalid batch size error, got {error}"
    );

    // Inspect the catalog to prove validation ran before any TPC-C DDL.
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).expect("open catalog");
    if let Some(snapshot) = catalog.load().expect("load catalog") {
        assert!(
            snapshot.table_by_name("warehouse").is_none(),
            "invalid load must not create the warehouse table"
        );
    }
}
