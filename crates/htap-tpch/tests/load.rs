use std::time::Instant;

use htap_catalog::local::LocalCatalogStore;
use htap_catalog::store::CatalogStore;
use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use htap_tpch::{
    ddl_statements, generate, load_dataset, query, LoadOptions, LoadReport, TABLE_NAMES,
};
use tempfile::TempDir;

fn dataset_lengths(dataset: &htap_tpch::Dataset) -> [usize; 8] {
    [
        dataset.region.len(),
        dataset.nation.len(),
        dataset.part.len(),
        dataset.supplier.len(),
        dataset.partsupp.len(),
        dataset.customer.len(),
        dataset.orders.len(),
        dataset.lineitem.len(),
    ]
}

#[test]
fn row_counts_copy_reports_and_round_trip_values() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let dataset = generate("0.01", 0).expect("generate SF 0.01 dataset");

    let customer = dataset
        .customer
        .iter()
        .find(|row| row.c_acctbal < 0)
        .expect("generated dataset contains a customer with negative account balance");
    let part = dataset
        .part
        .first()
        .expect("generated dataset contains parts");
    let lineitem = dataset
        .lineitem
        .first()
        .expect("generated dataset contains lineitems");
    let order = dataset
        .orders
        .iter()
        .find(|row| row.o_orderkey == lineitem.l_orderkey)
        .expect("generated lineitem references an order");

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

    let part_result = server
        .execute(&format!(
            "SELECT p_retailprice FROM part WHERE p_partkey = {}",
            part.p_partkey
        ))
        .expect("query sample part");
    match part_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(
                result.rows()[0].get(0).map(ToString::to_string),
                Some(htap_tpch::load::format_decimal(part.p_retailprice))
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let customer_result = server
        .execute(&format!(
            "SELECT c_custkey, c_acctbal FROM customer WHERE c_custkey = {}",
            customer.c_custkey
        ))
        .expect("query negative customer balance");
    match customer_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(
                result.rows()[0].get(0),
                Some(&Value::Int64(customer.c_custkey))
            );
            assert_eq!(
                result.rows()[0].get(1).map(ToString::to_string),
                Some(htap_tpch::load::format_decimal(customer.c_acctbal))
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let order_result = server
        .execute(&format!(
            "SELECT o_orderdate FROM orders WHERE o_orderkey = {}",
            order.o_orderkey
        ))
        .expect("query sample order");
    match order_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(
                result.rows()[0].get(0),
                Some(&Value::Timestamp(
                    htap_common::types::parse_date_to_timestamp_micros(&order.o_orderdate)
                        .expect("parse generated order date")
                ))
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }

    let lineitem_result = server
        .execute(&format!(
            "SELECT l_quantity, l_discount, l_tax, l_extendedprice, l_shipdate, \
             l_commitdate, l_receiptdate FROM lineitem \
             WHERE l_orderkey = {} AND l_linenumber = {}",
            lineitem.l_orderkey, lineitem.l_linenumber
        ))
        .expect("query sample lineitem");
    match lineitem_result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            let row = &result.rows()[0];
            assert_eq!(
                row.get(0).map(ToString::to_string),
                Some(format!("{}.00", lineitem.l_quantity))
            );
            assert_eq!(
                row.get(1).map(ToString::to_string),
                Some(htap_tpch::load::format_decimal(lineitem.l_discount))
            );
            assert_eq!(
                row.get(2).map(ToString::to_string),
                Some(htap_tpch::load::format_decimal(lineitem.l_tax))
            );
            assert_eq!(
                row.get(3).map(ToString::to_string),
                Some(htap_tpch::load::format_decimal(lineitem.l_extendedprice))
            );
            assert_eq!(
                row.get(4),
                Some(&Value::Timestamp(
                    htap_common::types::parse_date_to_timestamp_micros(&lineitem.l_shipdate)
                        .expect("parse generated ship date")
                ))
            );
            assert_eq!(
                row.get(5),
                Some(&Value::Timestamp(
                    htap_common::types::parse_date_to_timestamp_micros(&lineitem.l_commitdate)
                        .expect("parse generated commit date")
                ))
            );
            assert_eq!(
                row.get(6),
                Some(&Value::Timestamp(
                    htap_common::types::parse_date_to_timestamp_micros(&lineitem.l_receiptdate)
                        .expect("parse generated receipt date")
                ))
            );
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
fn invalid_batch_rows_does_not_create_tables() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let dataset = generate("0.01", 0).expect("generate SF 0.01 dataset");

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

    // Inspect the catalog to prove validation ran before any TPC-H DDL.
    let catalog = LocalCatalogStore::open(dir.path().join("catalog")).expect("open catalog");
    if let Some(snapshot) = catalog.load().expect("load catalog") {
        assert!(
            snapshot.table_by_name("region").is_none(),
            "invalid load must not create the region table"
        );
    }
}

#[test]
fn second_load_rejection() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let dataset = generate("0.01", 0).expect("generate SF 0.01 dataset");

    for ddl in ddl_statements() {
        server.execute(ddl).expect("create TPC-H table");
    }

    let error = load_dataset(&server, dir.path(), &dataset, &LoadOptions::default())
        .expect_err("load into existing tables must be rejected");

    assert!(
        matches!(error, HtapError::InvalidArgument(_)),
        "expected InvalidArgument, got {error:?}"
    );
    assert!(
        error.to_string().contains("already exists"),
        "expected existing-table error, got {error}"
    );

    let result = server
        .execute("SELECT count(*) FROM lineitem")
        .expect("count lineitems");
    match result {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            assert_eq!(result.rows()[0].get(0), Some(&Value::Int64(0)));
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

#[test]
#[ignore = "loads SF 0.01 and runs all 22 queries; slow in debug; run with --ignored"]
fn all_22_queries() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = LocalServer::open(dir.path()).expect("open local server");
    let dataset = generate("0.01", 0).expect("generate SF 0.01 dataset");

    load_dataset(&server, dir.path(), &dataset, &LoadOptions::default()).expect("load dataset");

    let query_numbers: Vec<u8> = match std::env::var("TPCH_QUERIES") {
        Ok(value) => value
            .split(',')
            .map(|number| {
                number
                    .trim()
                    .parse()
                    .expect("TPCH_QUERIES must contain comma-separated query numbers")
            })
            .collect(),
        Err(std::env::VarError::NotPresent) => (1..=22).collect(),
        Err(error) => panic!("read TPCH_QUERIES: {error}"),
    };

    for query_number in query_numbers {
        let sql = query(query_number, "0.01").expect("TPC-H query definition");
        eprintln!("TPC-H Query {query_number} starting");
        let started = Instant::now();
        let result = server.execute(&sql);
        let elapsed = started.elapsed();

        match result {
            Ok(StatementResult::Query(result)) => {
                eprintln!(
                    "TPC-H Query {query_number} completed in {elapsed:?} with {} rows",
                    result.num_rows()
                );
            }
            Ok(other) => {
                panic!("TPC-H Query {query_number} did not return a query result: {other:?}")
            }
            Err(error) => panic!("TPC-H Query {query_number} failed after {elapsed:?}: {error}"),
        }
    }
}
