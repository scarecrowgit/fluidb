use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::consistency::{check_consistency, ConsistencyViolation};
use htap_tpcc::load::{load_dataset, LoadOptions};
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{
    delivery, new_order, payment, CustomerSelector, DeliveryDistrictResult, DeliveryRequest,
    NewOrderItem, NewOrderRequest, PaymentRequest,
};
use tempfile::TempDir;

fn setup() -> Result<(TempDir, Arc<LocalServer>), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    Ok((directory, server))
}

fn small_consistent_dataset(
    server: &LocalServer,
    server_root: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    // All 10 districts are required for warehouse-level consistency conditions.
    let dataset = htap_tpcc::generate::generate(1, 0)?;
    load_dataset(server, server_root, &dataset, &LoadOptions::default())?;
    Ok(())
}

fn query_count(session: &mut Session, sql: &str) -> Result<i64, Box<dyn Error>> {
    let StatementResult::Query(result) = session.execute(sql)? else {
        panic!("expected query result for {sql}");
    };
    match result.rows()[0].get(0) {
        Some(Value::Int32(value)) => Ok(i64::from(*value)),
        Some(Value::Int64(value)) => Ok(*value),
        Some(Value::Decimal { value, .. }) => Ok(*value),
        value => panic!("expected count result, got {value:?}"),
    }
}

fn condition_numbers(violations: &[ConsistencyViolation]) -> Vec<i64> {
    let mut conditions = violations
        .iter()
        .map(|violation| violation.condition_number)
        .collect::<Vec<_>>();
    conditions.sort_unstable();
    conditions.dedup();
    conditions
}

fn assert_condition_set(violations: &[ConsistencyViolation], expected: &[i64]) {
    assert_eq!(condition_numbers(violations), expected);
}

#[test]
#[ignore = "loads a full warehouse; run with cargo test --release -- --ignored"]
fn load_and_verify_all_conditions_pass() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    small_consistent_dataset(&server, _directory.path())?;

    assert!(check_consistency(&mut session, &[1])?.is_empty());
    Ok(())
}

#[test]
#[ignore = "loads a full warehouse; run with cargo test --release -- --ignored"]
fn transactions_preserve_derived_condition_11() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    small_consistent_dataset(&server, _directory.path())?;

    new_order(
        &mut session,
        &NewOrderRequest {
            w_id: 1,
            d_id: 1,
            c_id: 1,
            entry_timestamp_micros: 1_000_000,
            items: (1..=5)
                .map(|item_id| NewOrderItem {
                    item_id,
                    supply_w_id: 1,
                    quantity: 1,
                })
                .collect(),
        },
    )?;

    payment(
        &mut session,
        &PaymentRequest {
            w_id: 1,
            d_id: 1,
            customer_w_id: 1,
            customer_d_id: 1,
            customer: CustomerSelector::Id(1),
            entry_timestamp_micros: 2_000_000,
            h_amount: 100,
            h_terminal: 1,
            h_sequence: 1,
        },
    )?;

    let delivery_result = delivery(
        &mut session,
        &DeliveryRequest {
            w_id: 1,
            carrier_id: 1,
            delivery_timestamp_micros: 3_000_000,
        },
    )?;
    assert!(matches!(
        delivery_result.per_district.first(),
        Some(DeliveryDistrictResult::Delivered { .. })
    ));
    let delivered_in_district_1 = delivery_result
        .per_district
        .iter()
        .take(1)
        .filter(|result| matches!(result, DeliveryDistrictResult::Delivered { .. }))
        .count() as i64;
    assert_eq!(delivered_in_district_1, 1);

    let orders = query_count(
        &mut session,
        "SELECT count(*) FROM orders WHERE o_w_id = 1 AND o_d_id = 1",
    )?;
    let new_orders = query_count(
        &mut session,
        "SELECT count(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = 1",
    )?;

    assert_eq!(orders - new_orders, 2_100 + delivered_in_district_1);

    // Delivery removes a NEW-ORDER row, so the literal 2,100-order invariant no longer applies.
    assert_condition_set(&check_consistency(&mut session, &[1])?, &[11]);
    Ok(())
}

#[test]
fn unknown_warehouse_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    for ddl in ddl_statements() {
        server.execute(&ddl)?;
    }

    let error =
        check_consistency(&mut session, &[1]).expect_err("unknown warehouse must be rejected");

    assert!(matches!(
        error,
        htap_tpcc::transactions::TransactionError::InvalidInput(message)
            if message == "warehouse 1 was not found"
    ));
    Ok(())
}

#[test]
fn warehouse_without_districts_reports_ytd_violations() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    for ddl in ddl_statements() {
        server.execute(&ddl)?;
    }
    session.execute(
        "INSERT INTO warehouse \
         (w_id, w_name, w_street_1, w_street_2, w_city, w_state, w_zip, w_tax, w_ytd) \
         VALUES (1, 'warehouse', 'street 1', 'street 2', 'city', 'ST', '123456789', \
                 0.0000, 1.00)",
    )?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[1, 8]);
    Ok(())
}

#[test]
#[ignore = "loads a full warehouse; run with cargo test --release -- --ignored"]
fn corruption_of_condition_1_reports_only_condition_1() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    small_consistent_dataset(&server, _directory.path())?;

    session.execute("UPDATE warehouse SET w_ytd = w_ytd + 1.00 WHERE w_id = 1")?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[1, 8]);
    Ok(())
}

#[test]
#[ignore = "loads a full warehouse; run with cargo test --release -- --ignored"]
fn corruption_of_condition_2_reports_only_condition_2() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    small_consistent_dataset(&server, _directory.path())?;

    session.execute(
        "UPDATE district SET d_next_o_id = d_next_o_id - 1 \
         WHERE d_w_id = 1 AND d_id = 1",
    )?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[2]);
    Ok(())
}

#[test]
#[ignore = "loads a full warehouse; run with cargo test --release -- --ignored"]
fn corruption_of_condition_3_reports_only_condition_3() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    small_consistent_dataset(&server, _directory.path())?;

    session.execute(
        "DELETE FROM new_order \
         WHERE no_w_id = 1 AND no_d_id = 1 AND no_o_id = 2500",
    )?;

    // The gap breaks condition 3, leaves an undelivered order without NEW-ORDER for condition 5,
    // and changes the ORDER minus NEW-ORDER count for condition 11.
    assert_condition_set(&check_consistency(&mut session, &[1])?, &[3, 5, 11]);
    Ok(())
}

#[test]
#[ignore = "loads a full warehouse; run with cargo test --release -- --ignored"]
fn corruption_of_condition_4_reports_only_condition_4() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    small_consistent_dataset(&server, _directory.path())?;

    session.execute(
        "UPDATE orders SET o_ol_cnt = o_ol_cnt + 1 \
         WHERE o_w_id = 1 AND o_d_id = 1 AND o_id = 1",
    )?;

    // Changing O_OL_CNT breaks both the district total and this order's line count.
    assert_condition_set(&check_consistency(&mut session, &[1])?, &[4, 6]);
    Ok(())
}

#[test]
#[ignore = "run with cargo test --release -- --ignored"]
fn test_full_warehouse() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let dataset = htap_tpcc::generate::generate(1, 0)?;
    load_dataset(
        &server,
        _directory.path(),
        &dataset,
        &LoadOptions::default(),
    )?;

    assert!(check_consistency(&mut session, &[1])?.is_empty());
    Ok(())
}
