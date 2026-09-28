use std::collections::BTreeSet;
use std::sync::Arc;

use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::LocalServer;
use htap_sql::result::StatementResult;
use htap_tpch::refresh::{
    apply_one_order_insert, generate_rf1_rows, generate_rf2_plan, rf1_new_sales, rf2_old_sales,
};
use htap_tpch::{ddl_statements, generate, load_dataset, LoadOptions};
use tempfile::TempDir;

fn count(server: &Arc<LocalServer>, table_name: &str) -> i64 {
    match server
        .execute(&format!("SELECT count(*) FROM {table_name}"))
        .expect("count table rows")
    {
        StatementResult::Query(result) => {
            assert_eq!(result.num_rows(), 1);
            match result.rows()[0].get(0) {
                Some(Value::Int64(value)) => *value,
                other => panic!("expected count value, got {other:?}"),
            }
        }
        other => panic!("expected query result, got {other:?}"),
    }
}

fn load_sf_001(server: &Arc<LocalServer>, path: &std::path::Path) {
    let dataset = generate("0.01", 0).expect("generate SF 0.01 dataset");
    load_dataset(server, path, &dataset, &LoadOptions::default()).expect("load dataset");
}

#[test]
fn test_rf1_inserts_orders_and_lineitems_with_correct_counts() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = Arc::new(LocalServer::open(dir.path()).expect("open local server"));
    load_sf_001(&server, dir.path());

    let rows = generate_rf1_rows("0.01", 1, 42).expect("generate RF1 rows");
    let orders_before = count(&server, "orders");
    let lineitems_before = count(&server, "lineitem");

    let mut session = server.open_session().expect("open session");
    rf1_new_sales(&mut session, "0.01", 1, 42).expect("apply RF1 stream");

    assert_eq!(
        count(&server, "orders"),
        orders_before + rows.orders.len() as i64
    );
    assert_eq!(
        count(&server, "lineitem"),
        lineitems_before + rows.lineitems.len() as i64
    );
}

#[test]
fn test_rf2_deletes_orders_and_makes_rows_gone() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = Arc::new(LocalServer::open(dir.path()).expect("open local server"));
    load_sf_001(&server, dir.path());

    let order_keys = generate_rf2_plan("0.01", 1).expect("generate RF2 plan");
    let keys_sql = order_keys
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let orders_before = count(&server, "orders");
    let lineitems_before = count(&server, "lineitem");

    let deleted_lineitems = match server
        .execute(&format!(
            "SELECT count(*) FROM lineitem WHERE l_orderkey IN ({keys_sql})"
        ))
        .expect("count lineitems selected by RF2")
    {
        StatementResult::Query(result) => match result.rows()[0].get(0) {
            Some(Value::Int64(value)) => *value,
            other => panic!("expected count value, got {other:?}"),
        },
        other => panic!("expected query result, got {other:?}"),
    };

    let mut session = server.open_session().expect("open session");
    rf2_old_sales(&mut session, "0.01", 1).expect("apply RF2 stream");

    assert_eq!(
        count(&server, "orders"),
        orders_before - order_keys.len() as i64
    );
    assert_eq!(
        count(&server, "lineitem"),
        lineitems_before - deleted_lineitems
    );

    for table_name in ["orders", "lineitem"] {
        let key_column = if table_name == "orders" {
            "o_orderkey"
        } else {
            "l_orderkey"
        };
        match server
            .execute(&format!(
                "SELECT count(*) FROM {table_name} WHERE {key_column} IN ({keys_sql})"
            ))
            .expect("verify RF2 rows were deleted")
        {
            StatementResult::Query(result) => {
                assert_eq!(result.rows()[0].get(0), Some(&Value::Int64(0)));
            }
            other => panic!("expected query result, got {other:?}"),
        }
    }
}

#[test]
fn test_rf1_then_rf2_restores_counts_with_quarter_shift() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = Arc::new(LocalServer::open(dir.path()).expect("open local server"));
    load_sf_001(&server, dir.path());

    let orders_before = count(&server, "orders");
    let rf1_rows = generate_rf1_rows("0.01", 1, 42).expect("generate RF1 rows");
    let rf1_keys: BTreeSet<_> = rf1_rows.orders.iter().map(|row| row.o_orderkey).collect();
    let rf2_keys: BTreeSet<_> = generate_rf2_plan("0.01", 1)
        .expect("generate RF2 plan")
        .into_iter()
        .collect();

    assert!(rf1_keys.iter().all(|key| (9..=16).contains(&(key % 32))));
    assert!(rf2_keys.iter().all(|key| (1..=8).contains(&(key % 32))));

    let mut session = server.open_session().expect("open session");
    rf1_new_sales(&mut session, "0.01", 1, 42).expect("apply RF1 stream");
    rf2_old_sales(&mut session, "0.01", 1).expect("apply RF2 stream");

    assert_eq!(count(&server, "orders"), orders_before);

    for order_key in rf1_keys {
        match server
            .execute(&format!(
                "SELECT count(*) FROM orders WHERE o_orderkey = {order_key}"
            ))
            .expect("verify RF1 order exists")
        {
            StatementResult::Query(result) => {
                assert_eq!(result.rows()[0].get(0), Some(&Value::Int64(1)));
            }
            other => panic!("expected query result, got {other:?}"),
        }
    }

    for order_key in rf2_keys {
        match server
            .execute(&format!(
                "SELECT count(*) FROM orders WHERE o_orderkey = {order_key}"
            ))
            .expect("verify RF2 order is gone")
        {
            StatementResult::Query(result) => {
                assert_eq!(result.rows()[0].get(0), Some(&Value::Int64(0)));
            }
            other => panic!("expected query result, got {other:?}"),
        }
    }
}

#[test]
fn test_partial_failure_rolls_back_without_committing_half_order() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = Arc::new(LocalServer::open(dir.path()).expect("open local server"));

    for ddl in ddl_statements() {
        server.execute(ddl).expect("create TPC-H table");
    }

    let rows = generate_rf1_rows("0.01", 1, 42).expect("generate RF1 rows");
    let order = rows.orders.first().expect("RF1 contains orders").clone();
    let mut lineitems: Vec<_> = rows
        .lineitems
        .iter()
        .filter(|lineitem| lineitem.l_orderkey == order.o_orderkey)
        .cloned()
        .collect();
    assert!(!lineitems.is_empty(), "RF1 order has lineitems");

    lineitems[0].l_shipdate = "not-a-date".to_string();

    let mut session = server.open_session().expect("open session");
    let error = apply_one_order_insert(&mut session, &order, &lineitems)
        .expect_err("invalid lineitem date must fail");

    assert!(
        matches!(
            error,
            HtapError::InvalidArgument(_) | HtapError::Corruption(_)
        ),
        "expected insert error, got {error:?}"
    );

    assert_eq!(count(&server, "orders"), 0);
    assert_eq!(count(&server, "lineitem"), 0);
}

#[test]
fn test_out_of_range_streams_are_rejected() {
    let dir = TempDir::new().expect("create temporary directory");
    let server = Arc::new(LocalServer::open(dir.path()).expect("open local server"));
    let mut session = server.open_session().expect("open session");

    for stream in [0, 3_001] {
        assert!(
            matches!(
                rf1_new_sales(&mut session, "0.01", stream, 42),
                Err(HtapError::InvalidArgument(_))
            ),
            "RF1 stream {stream} must be rejected"
        );
    }

    for stream in [0, 1_001] {
        assert!(
            matches!(
                rf2_old_sales(&mut session, "0.01", stream),
                Err(HtapError::InvalidArgument(_))
            ),
            "RF2 stream {stream} must be rejected"
        );
    }
}
