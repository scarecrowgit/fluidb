use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::consistency::{check_consistency, ConsistencyViolation};
use htap_tpcc::generate::{
    Customer, Dataset, District, History, Item, NewOrder, OrderLine, Orders, Stock, Warehouse,
};
use htap_tpcc::history::build_h_id;
use htap_tpcc::load::{load_dataset, LoadOptions};
use htap_tpcc::transactions::{
    delivery, new_order, payment, CustomerSelector, DeliveryDistrictResult, DeliveryRequest,
    NewOrderItem, NewOrderRequest, PaymentRequest,
};
use tempfile::TempDir;

const CUSTOMER_COUNT: i64 = 5;
const DELIVERED_ORDER_COUNT: i64 = 2_100;
const NEW_ORDER_COUNT: i64 = 3;
const HISTORY_AMOUNT: i64 = 1_000;
const DELIVERED_LINE_AMOUNT: i64 = 100;

fn setup() -> Result<(TempDir, Arc<LocalServer>), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    Ok((directory, server))
}

fn small_consistent_dataset() -> Dataset {
    let delivered_per_customer = DELIVERED_ORDER_COUNT / CUSTOMER_COUNT;
    let delivered_amount = delivered_per_customer * DELIVERED_LINE_AMOUNT;

    let warehouse = Warehouse {
        w_id: 1,
        w_name: "Warehouse".into(),
        w_street_1: "Street 1".into(),
        w_street_2: "Street 2".into(),
        w_city: "City".into(),
        w_state: "ST".into(),
        w_zip: "123456789".into(),
        w_tax: 0,
        w_ytd: CUSTOMER_COUNT * HISTORY_AMOUNT,
    };
    let district = District {
        d_id: 1,
        d_w_id: 1,
        d_name: "District".into(),
        d_street_1: "Street 1".into(),
        d_street_2: "Street 2".into(),
        d_city: "City".into(),
        d_state: "ST".into(),
        d_zip: "123456789".into(),
        d_tax: 0,
        d_ytd: CUSTOMER_COUNT * HISTORY_AMOUNT,
        d_next_o_id: DELIVERED_ORDER_COUNT + NEW_ORDER_COUNT + 1,
    };

    let item = (1..=CUSTOMER_COUNT)
        .map(|id| Item {
            i_id: id,
            i_im_id: id,
            i_name: format!("Item {id}"),
            i_price: 100,
            i_data: "data".into(),
        })
        .collect();
    let stock = (1..=CUSTOMER_COUNT)
        .map(|id| Stock {
            s_i_id: id,
            s_w_id: 1,
            s_quantity: 100,
            s_dist_01: "distribution information".into(),
            s_dist_02: "distribution information".into(),
            s_dist_03: "distribution information".into(),
            s_dist_04: "distribution information".into(),
            s_dist_05: "distribution information".into(),
            s_dist_06: "distribution information".into(),
            s_dist_07: "distribution information".into(),
            s_dist_08: "distribution information".into(),
            s_dist_09: "distribution information".into(),
            s_dist_10: "distribution information".into(),
            s_ytd: 0,
            s_order_cnt: 0,
            s_remote_cnt: 0,
            s_data: "data".into(),
        })
        .collect();
    let customer = (1..=CUSTOMER_COUNT)
        .map(|id| Customer {
            c_id: id,
            c_d_id: 1,
            c_w_id: 1,
            c_first: format!("Customer{id}"),
            c_middle: "OE".into(),
            c_last: format!("Last{id}"),
            c_street_1: "Street 1".into(),
            c_street_2: "Street 2".into(),
            c_city: "City".into(),
            c_state: "ST".into(),
            c_zip: "123456789".into(),
            c_phone: "1234567890123456".into(),
            c_since: "2000-01-01 00:00:00".into(),
            c_credit: "GC".into(),
            c_credit_lim: 5_000_000,
            c_discount: 0,
            c_balance: delivered_amount - HISTORY_AMOUNT,
            c_ytd_payment: HISTORY_AMOUNT,
            c_payment_cnt: 1,
            c_delivery_cnt: delivered_per_customer,
            c_data: "data".into(),
        })
        .collect();
    let history = (1..=CUSTOMER_COUNT)
        .map(|id| History {
            h_id: build_h_id(0, (id - 1) as u64).unwrap(),
            h_c_id: id,
            h_c_d_id: 1,
            h_c_w_id: 1,
            h_d_id: 1,
            h_w_id: 1,
            h_date: "2000-01-01 00:00:00".into(),
            h_amount: HISTORY_AMOUNT,
            h_data: "initial payment".into(),
        })
        .collect();
    let orders = (1..=(DELIVERED_ORDER_COUNT + NEW_ORDER_COUNT))
        .map(|id| {
            let delivered = id <= DELIVERED_ORDER_COUNT;
            Orders {
                o_id: id,
                o_d_id: 1,
                o_w_id: 1,
                o_c_id: (id - 1) % CUSTOMER_COUNT + 1,
                o_entry_d: "2000-01-01 00:00:00".into(),
                o_carrier_id: delivered.then_some(1),
                o_ol_cnt: 1,
                o_all_local: 1,
            }
        })
        .collect::<Vec<_>>();
    let order_line = orders
        .iter()
        .map(|order| {
            let delivered = order.o_id <= DELIVERED_ORDER_COUNT;
            OrderLine {
                ol_o_id: order.o_id,
                ol_d_id: 1,
                ol_w_id: 1,
                ol_number: 1,
                ol_i_id: (order.o_id - 1) % CUSTOMER_COUNT + 1,
                ol_supply_w_id: 1,
                ol_delivery_d: delivered.then_some("2000-01-01 00:00:00".into()),
                ol_quantity: 5,
                ol_amount: if delivered {
                    DELIVERED_LINE_AMOUNT
                } else {
                    100
                },
                ol_dist_info: "distribution information".into(),
            }
        })
        .collect();
    let new_order = ((DELIVERED_ORDER_COUNT + 1)..=(DELIVERED_ORDER_COUNT + NEW_ORDER_COUNT))
        .map(|id| NewOrder {
            no_o_id: id,
            no_d_id: 1,
            no_w_id: 1,
        })
        .collect();

    Dataset {
        warehouse: vec![warehouse],
        district: vec![district],
        item,
        stock,
        customer,
        history,
        orders,
        order_line,
        new_order,
    }
}

fn load_small_consistent_dataset(
    server: &LocalServer,
    server_root: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    load_dataset(
        server,
        server_root,
        &small_consistent_dataset(),
        &LoadOptions::default(),
    )?;
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
    conditions
}

fn assert_condition_set(violations: &[ConsistencyViolation], expected: &[i64]) {
    assert_eq!(condition_numbers(violations), expected);
}

#[test]
fn test_small_warehouse_all_conditions_pass() -> Result<(), Box<dyn Error>> {
    let (directory, server) = setup()?;
    let mut session = server.open_session()?;
    load_small_consistent_dataset(&server, directory.path())?;

    assert!(check_consistency(&mut session, &[1])?.is_empty());
    Ok(())
}

#[test]
fn test_small_warehouse_conditions_after_transactions() -> Result<(), Box<dyn Error>> {
    let (directory, server) = setup()?;
    let mut session = server.open_session()?;
    load_small_consistent_dataset(&server, directory.path())?;

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
    let delivered = delivery_result
        .per_district
        .iter()
        .filter(|result| matches!(result, DeliveryDistrictResult::Delivered { .. }))
        .count() as i64;

    let orders = query_count(
        &mut session,
        "SELECT count(*) FROM orders WHERE o_w_id = 1 AND o_d_id = 1",
    )?;
    let new_orders = query_count(
        &mut session,
        "SELECT count(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = 1",
    )?;
    assert_eq!(orders - new_orders, DELIVERED_ORDER_COUNT + delivered);

    // Delivery removes a NEW-ORDER row, so the literal 2,100-order invariant no longer applies.
    assert_condition_set(&check_consistency(&mut session, &[1])?, &[11]);
    Ok(())
}

#[test]
fn test_small_warehouse_corruption_condition_1() -> Result<(), Box<dyn Error>> {
    let (directory, server) = setup()?;
    let mut session = server.open_session()?;
    load_small_consistent_dataset(&server, directory.path())?;

    session.execute("UPDATE warehouse SET w_ytd = w_ytd + 1.00 WHERE w_id = 1")?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[1, 8]);
    Ok(())
}

#[test]
fn test_small_warehouse_corruption_condition_2() -> Result<(), Box<dyn Error>> {
    let (directory, server) = setup()?;
    let mut session = server.open_session()?;
    load_small_consistent_dataset(&server, directory.path())?;

    session.execute(
        "UPDATE district SET d_next_o_id = d_next_o_id + 1 WHERE d_w_id = 1 AND d_id = 1",
    )?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[2]);
    Ok(())
}

#[test]
fn test_small_warehouse_corruption_condition_3() -> Result<(), Box<dyn Error>> {
    let (directory, server) = setup()?;
    let mut session = server.open_session()?;
    load_small_consistent_dataset(&server, directory.path())?;

    session
        .execute("DELETE FROM new_order WHERE no_w_id = 1 AND no_d_id = 1 AND no_o_id = 2102")?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[3, 5, 11]);
    Ok(())
}

#[test]
fn test_small_warehouse_corruption_condition_4() -> Result<(), Box<dyn Error>> {
    let (directory, server) = setup()?;
    let mut session = server.open_session()?;
    load_small_consistent_dataset(&server, directory.path())?;

    session.execute(
        "UPDATE orders SET o_ol_cnt = o_ol_cnt + 1 \
         WHERE o_w_id = 1 AND o_d_id = 1 AND o_id = 1",
    )?;

    assert_condition_set(&check_consistency(&mut session, &[1])?, &[4, 6]);
    Ok(())
}
