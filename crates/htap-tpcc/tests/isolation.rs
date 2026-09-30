use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_common::HtapError;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{
    delivery, delivery_one_district, new_order, order_status, payment, CustomerSelector,
    DeliveryDistrictResult, DeliveryRequest, NewOrderItem, NewOrderRequest, OrderStatusRequest,
    PaymentRequest, TransactionError,
};
use tempfile::TempDir;

const MAX_RETRY_ATTEMPTS: usize = 100;

fn setup() -> Result<(TempDir, Arc<LocalServer>), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let server = Arc::new(LocalServer::open(directory.path())?);
    for ddl in ddl_statements() {
        server.execute(&ddl)?;
    }
    Ok((directory, server))
}

fn execute(session: &mut Session, sql: &str) -> Result<(), Box<dyn Error>> {
    session.execute(sql)?;
    Ok(())
}

fn query_i64(session: &mut Session, sql: &str) -> Result<i64, Box<dyn Error>> {
    let StatementResult::Query(result) = session.execute(sql)? else {
        panic!("expected query result for {sql}");
    };
    match result.rows()[0].get(0) {
        Some(Value::Int32(value)) => Ok(i64::from(*value)),
        Some(Value::Int64(value)) => Ok(*value),
        Some(Value::Decimal { value, .. }) => Ok(*value),
        Some(Value::Timestamp(value)) => Ok(*value),
        value => panic!("expected integer-compatible result, got {value:?}"),
    }
}

fn insert_warehouse(session: &mut Session, w_id: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO warehouse \
             (w_id, w_name, w_street_1, w_street_2, w_city, w_state, w_zip, w_tax, w_ytd) \
             VALUES ({w_id}, 'W{w_id}', 'street', 'suite', 'city', 'ST', '123456789', \
                     0.0000, 0.00)"
        ),
    )
}

fn insert_district(session: &mut Session, w_id: i64, d_id: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO district \
             (d_id, d_w_id, d_name, d_street_1, d_street_2, d_city, d_state, d_zip, \
              d_tax, d_ytd, d_next_o_id) \
             VALUES ({d_id}, {w_id}, 'D{d_id}', 'street', 'suite', 'city', 'ST', \
                     '123456789', 0.0000, 0.00, 3001)"
        ),
    )
}

fn insert_customer(
    session: &mut Session,
    w_id: i64,
    d_id: i64,
    c_id: i64,
    balance: &str,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO customer \
             (c_id, c_d_id, c_w_id, c_first, c_middle, c_last, c_street_1, c_street_2, \
              c_city, c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
              c_balance, c_ytd_payment, c_payment_cnt, c_delivery_cnt, c_data) \
             VALUES ({c_id}, {d_id}, {w_id}, 'First{c_id}', 'OE', 'Last{c_id}', \
                     'street', 'suite', 'city', 'ST', '123456789', '1234567890123456', \
                     DATE '2000-01-01', 'GC', 50000.00, 0.0000, {balance}, 0.00, 0, 0, 'data')"
        ),
    )
}

fn insert_item_and_stock(
    session: &mut Session,
    item_id: i64,
    quantity: i64,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO item (i_id, i_im_id, i_name, i_price, i_data) \
             VALUES ({item_id}, 1, 'item-{item_id}', 10.00, 'data')"
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO stock \
             (s_i_id, s_w_id, s_quantity, s_dist_01, s_dist_02, s_dist_03, s_dist_04, \
              s_dist_05, s_dist_06, s_dist_07, s_dist_08, s_dist_09, s_dist_10, \
              s_ytd, s_order_cnt, s_remote_cnt, s_data) \
             VALUES ({item_id}, 1, {quantity}, 'dist', 'dist', 'dist', 'dist', 'dist', \
                     'dist', 'dist', 'dist', 'dist', 'dist', 0, 0, 0, 'data')"
        ),
    )
}

fn seed_new_order_data(session: &mut Session) -> Result<(), Box<dyn Error>> {
    insert_warehouse(session, 1)?;
    insert_district(session, 1, 1)?;
    insert_customer(session, 1, 1, 1, "100.00")?;
    for item_id in 1..=5 {
        insert_item_and_stock(session, item_id, 100)?;
    }
    Ok(())
}

fn new_order_request(quantity: i64, timestamp: i64) -> NewOrderRequest {
    NewOrderRequest {
        w_id: 1,
        d_id: 1,
        c_id: 1,
        entry_timestamp_micros: timestamp,
        items: (1..=5)
            .map(|item_id| NewOrderItem {
                item_id,
                supply_w_id: 1,
                quantity,
            })
            .collect(),
    }
}

fn payment_request(amount: i64, sequence: u64) -> PaymentRequest {
    PaymentRequest {
        w_id: 1,
        d_id: 1,
        customer_w_id: 1,
        customer_d_id: 1,
        customer: CustomerSelector::Id(1),
        entry_timestamp_micros: 1_715_000_000_000_000 + sequence as i64,
        h_amount: amount,
        h_terminal: 1,
        h_sequence: sequence,
    }
}

fn retry_new_order(
    session: &mut Session,
    request: &NewOrderRequest,
) -> htap_tpcc::transactions::NewOrderResult {
    for _ in 0..MAX_RETRY_ATTEMPTS {
        match new_order(session, request) {
            Ok(result) => return result,
            Err(TransactionError::Conflict) => continue,
            Err(error) => panic!("unexpected New-Order error: {error}"),
        }
    }
    panic!("New-Order exhausted {MAX_RETRY_ATTEMPTS} attempts due to conflicts");
}

fn retry_payment(session: &mut Session, request: &PaymentRequest) {
    for _ in 0..MAX_RETRY_ATTEMPTS {
        match payment(session, request) {
            Ok(_) => return,
            Err(TransactionError::Conflict) => continue,
            Err(error) => panic!("unexpected Payment error: {error}"),
        }
    }
    panic!("Payment exhausted {MAX_RETRY_ATTEMPTS} attempts due to conflicts");
}

fn insert_delivery_order(
    session: &mut Session,
    district_id: i64,
    order_id: i64,
    customer_id: i64,
) -> Result<(), Box<dyn Error>> {
    insert_customer(session, 1, district_id, customer_id, "0.00")?;
    execute(
        session,
        &format!(
            "INSERT INTO orders \
             (o_id, o_d_id, o_w_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) \
             VALUES ({order_id}, {district_id}, 1, {customer_id}, 1715000000000000, NULL, 1, 1)"
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO new_order (no_o_id, no_d_id, no_w_id) \
             VALUES ({order_id}, {district_id}, 1)"
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO order_line \
             (ol_o_id, ol_d_id, ol_w_id, ol_number, ol_i_id, ol_supply_w_id, \
              ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) \
             VALUES ({order_id}, {district_id}, 1, 1, 1, 1, NULL, 1, 10.00, 'dist')"
        ),
    )
}

fn stage_new_order(
    session: &mut Session,
    request: &NewOrderRequest,
    invalid_last_item: bool,
) -> Result<i64, Box<dyn Error>> {
    let order_id = query_i64(
        session,
        &format!(
            "SELECT d_next_o_id FROM district WHERE d_w_id = {} AND d_id = {}",
            request.w_id, request.d_id
        ),
    )?;
    execute(
        session,
        &format!(
            "UPDATE district SET d_next_o_id = d_next_o_id + 1 \
             WHERE d_w_id = {} AND d_id = {}",
            request.w_id, request.d_id
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO orders \
             (o_id, o_d_id, o_w_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) \
             VALUES ({order_id}, {}, {}, {}, {}, NULL, {}, 1)",
            request.d_id,
            request.w_id,
            request.c_id,
            request.entry_timestamp_micros,
            request.items.len(),
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO new_order (no_o_id, no_d_id, no_w_id) \
             VALUES ({order_id}, {}, {})",
            request.d_id, request.w_id
        ),
    )?;

    for (index, item) in request.items.iter().enumerate() {
        let item_id = if invalid_last_item && index + 1 == request.items.len() {
            999
        } else {
            item.item_id
        };
        let StatementResult::Query(item_result) =
            session.execute(&format!("SELECT i_price FROM item WHERE i_id = {item_id}"))?
        else {
            panic!("expected item query result");
        };
        if item_result.num_rows() == 0 {
            break;
        }
        execute(
            session,
            &format!(
                "UPDATE stock SET s_quantity = s_quantity - {} \
                 WHERE s_w_id = {} AND s_i_id = {item_id}",
                item.quantity, item.supply_w_id
            ),
        )?;
        execute(
            session,
            &format!(
                "INSERT INTO order_line \
                 (ol_o_id, ol_d_id, ol_w_id, ol_number, ol_i_id, ol_supply_w_id, \
                  ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) \
                 VALUES ({order_id}, {}, {}, {}, {item_id}, {}, NULL, {}, 10.00, 'dist')",
                request.d_id,
                request.w_id,
                index + 1,
                item.supply_w_id,
                item.quantity,
            ),
        )?;
    }
    Ok(order_id)
}

fn stage_delivery(session: &mut Session, timestamp: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        "DELETE FROM new_order WHERE no_w_id = 1 AND no_d_id = 1 AND no_o_id = 3001",
    )?;
    execute(
        session,
        "UPDATE orders SET o_carrier_id = 1 WHERE o_w_id = 1 AND o_d_id = 1 AND o_id = 3001",
    )?;
    execute(
        session,
        &format!(
            "UPDATE order_line SET ol_delivery_d = {timestamp} \
             WHERE ol_w_id = 1 AND ol_d_id = 1 AND ol_o_id = 3001"
        ),
    )?;
    execute(
        session,
        "UPDATE customer SET c_balance = c_balance + 10.00, \
         c_delivery_cnt = c_delivery_cnt + 1 \
         WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1",
    )
}

// Spec 3.4.2.1: T2 reads Order-Status while T1's New-Order remains uncommitted.
// Fails under read-uncommitted; T2 would see T1's uncommitted order.
#[test]
fn isolation_test_1_new_order_then_order_status() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_data(&mut seed)?;

    let t0 = new_order(&mut seed, &new_order_request(1, 1))?;
    let t1_request = new_order_request(1, 2);

    t1.begin()?;
    let t1_order_id = stage_new_order(&mut t1, &t1_request, false)?;

    let status_before_commit = order_status(
        &mut t2,
        &OrderStatusRequest {
            w_id: 1,
            d_id: 1,
            customer: CustomerSelector::Id(1),
            entry_timestamp_micros: 3,
        },
    )?;
    assert_eq!(status_before_commit.order_id, t0.order_id);

    t1.commit()?;

    let status_after_commit = order_status(
        &mut t2,
        &OrderStatusRequest {
            w_id: 1,
            d_id: 1,
            customer: CustomerSelector::Id(1),
            entry_timestamp_micros: 4,
        },
    )?;
    assert_eq!(status_after_commit.order_id, t1_order_id);
    assert_eq!(
        status_after_commit.order_lines.len(),
        t1_request.items.len()
    );
    Ok(())
}

// Spec 3.4.2.2 steps: T0 commits New-Order, T1 starts an invalid New-Order, and T2 runs
// Order-Status before T1 rolls back.
// OCC adaptation: T2 reads a separate snapshot while T1's incomplete changes remain uncommitted.
// This fails under read-uncommitted isolation, which can expose T1's rolled-back order.
// Acceptable outcome: T2 sees T0's order and never T1's rolled-back order.
#[test]
fn isolation_test_2_rollback_is_not_visible_to_order_status() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_data(&mut seed)?;
    let original = new_order(&mut seed, &new_order_request(1, 1))?;

    t1.begin()?;
    stage_new_order(&mut t1, &new_order_request(1, 2), true)?;

    let status = order_status(
        &mut t2,
        &OrderStatusRequest {
            w_id: 1,
            d_id: 1,
            customer: CustomerSelector::Id(1),
            entry_timestamp_micros: 3,
        },
    )?;
    t1.rollback()?;

    assert_eq!(status.order_id, original.order_id);
    Ok(())
}

// Spec 3.4.2.3: T1 and T2 write D_NEXT_O_ID from the same snapshot.
// Fails under no-conflict-detection; both would increment D_NEXT_O_ID to 3002 (lost update).
#[test]
fn isolation_test_3_two_new_orders_have_consecutive_ids() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_data(&mut seed)?;

    let t1_request = new_order_request(1, 1);
    t1.begin()?;
    assert_eq!(stage_new_order(&mut t1, &t1_request, false)?, 3001);

    let t2_order = new_order(&mut t2, &new_order_request(1, 2))?;
    assert_eq!(t2_order.order_id, 3001);

    assert!(matches!(t1.commit(), Err(HtapError::Conflict(_))));

    let retried_t1_order = retry_new_order(&mut t1, &t1_request);
    assert_eq!(retried_t1_order.order_id, 3002);
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT d_next_o_id FROM district WHERE d_w_id = 1 AND d_id = 1"
        )?,
        3003
    );
    Ok(())
}

// Spec 3.4.2.4 steps: T1 starts an invalid New-Order and remains uncommitted while T2 starts
// a valid New-Order for the same district, then T1 rolls back.
// This detects dirty-write and abort-leak models.
// Acceptable outcome: T2 receives the base order ID, with no gap from T1's rollback.
#[test]
fn isolation_test_4_failed_new_order_does_not_consume_order_id() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_data(&mut seed)?;

    t1.begin()?;
    stage_new_order(&mut t1, &new_order_request(1, 1), true)?;

    let t2_order = new_order(&mut t2, &new_order_request(1, 2))?;
    assert_eq!(t2_order.order_id, 3001);

    t1.rollback()?;
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT d_next_o_id FROM district WHERE d_w_id = 1 AND d_id = 1"
        )?,
        3002
    );
    Ok(())
}

// Spec 3.4.2.5: T1 Delivery and T2 Payment write the same customer balance.
// Fails under no-conflict-detection; both would update c_balance but T1's stale snapshot (0)
// would overwrite Payment's effect.
#[test]
fn isolation_test_5_delivery_then_payment_updates_both_values() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    insert_warehouse(&mut seed, 1)?;
    insert_district(&mut seed, 1, 1)?;
    insert_delivery_order(&mut seed, 1, 3001, 1)?;

    t1.begin()?;
    stage_delivery(&mut t1, 1)?;

    assert!(payment(&mut t2, &payment_request(100, 1)).is_ok());
    assert!(matches!(t1.commit(), Err(HtapError::Conflict(_))));

    delivery(
        &mut t2,
        &DeliveryRequest {
            w_id: 1,
            carrier_id: 1,
            delivery_timestamp_micros: 1,
        },
    )?;

    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT c_balance FROM customer WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1"
        )?,
        900
    );
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT c_delivery_cnt FROM customer WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1"
        )?,
        1
    );
    Ok(())
}

// Spec 3.4.2.6 steps: T1 starts Delivery and remains uncommitted while T2 runs Payment for
// the same customer, then T1 rolls back.
// This fails under read-uncommitted isolation or dirty-write models that retain Delivery effects.
// Acceptable outcome: only Payment persists; Delivery changes and its NEW-ORDER deletion do not.
#[test]
fn isolation_test_6_rolled_back_delivery_has_no_customer_effect() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    insert_warehouse(&mut seed, 1)?;
    insert_district(&mut seed, 1, 1)?;
    insert_delivery_order(&mut seed, 1, 3001, 1)?;

    t1.begin()?;
    stage_delivery(&mut t1, 1)?;

    assert!(payment(&mut t2, &payment_request(100, 1)).is_ok());

    t1.rollback()?;

    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT c_balance FROM customer WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1"
        )?,
        -100
    );
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT c_delivery_cnt FROM customer WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1"
        )?,
        0
    );
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT COUNT(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = 1 AND no_o_id = 3001"
        )?,
        1
    );
    Ok(())
}

// Spec 3.4.2.7 steps: T1 reads item prices, T2 starts New-Order and reads its first price,
// then T3 updates the prices before T2 completes.
// OCC adaptation: T2 retains its fixed snapshot; its complete transaction would retry on conflict.
// This fails under read-committed isolation, which can expose a mixture of old and new prices.
// Acceptable outcome: every T2 order-line amount uses one price snapshot, never a mixture.
#[test]
fn isolation_test_7_new_order_uses_a_fixed_price_snapshot() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    let mut t3 = server.open_session()?;
    seed_new_order_data(&mut seed)?;

    t1.begin()?;
    assert_eq!(
        query_i64(&mut t1, "SELECT i_price FROM item WHERE i_id = 1")?,
        1_000
    );

    t2.begin()?;
    assert_eq!(
        query_i64(&mut t2, "SELECT i_price FROM item WHERE i_id = 1")?,
        1_000
    );

    t3.begin()?;
    execute(
        &mut t3,
        "UPDATE item SET i_price = 20.00 WHERE i_id >= 1 AND i_id <= 5",
    )?;
    t3.commit()?;

    let prices = (1..=5)
        .map(|item_id| {
            query_i64(
                &mut t2,
                &format!("SELECT i_price FROM item WHERE i_id = {item_id}"),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    t2.rollback()?;
    t1.rollback()?;

    assert!(prices.iter().all(|price| *price == 1_000));

    let result = retry_new_order(&mut seed, &new_order_request(1, 1));
    assert!(result.order_lines.iter().all(|line| line.amount == 2_000));
    Ok(())
}

// Spec 3.4.2 test 8 adapted for SI: the only acceptable result is that T1's fixed snapshot still
// has no NEW-ORDER row after T2 commits one.
#[test]
fn isolation_test_8_delivery_snapshot_has_no_phantom_new_order() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_data(&mut t1)?;

    t1.begin()?;
    assert_eq!(
        query_i64(
            &mut t1,
            "SELECT COUNT(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = 1"
        )?,
        0
    );
    new_order(&mut t2, &new_order_request(1, 1))?;
    assert_eq!(
        query_i64(
            &mut t1,
            "SELECT COUNT(*) FROM new_order WHERE no_w_id = 1 AND no_d_id = 1"
        )?,
        0
    );
    t1.rollback()?;
    Ok(())
}

// Spec 3.4.2 test 9 adapted for SI: T1's fixed snapshot continues to identify the same latest
// ORDER after T2 commits a newer order for the customer.
#[test]
fn isolation_test_9_order_status_snapshot_has_no_order_phantom() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut t1 = server.open_session()?;
    let mut t2 = server.open_session()?;
    seed_new_order_data(&mut t1)?;
    let original = new_order(&mut t1, &new_order_request(1, 1))?;

    t1.begin()?;
    assert_eq!(
        query_i64(
            &mut t1,
            "SELECT o_id FROM orders WHERE o_w_id = 1 AND o_d_id = 1 AND o_c_id = 1 \
             ORDER BY o_id DESC LIMIT 1"
        )?,
        original.order_id
    );
    new_order(&mut t2, &new_order_request(1, 2))?;
    assert_eq!(
        query_i64(
            &mut t1,
            "SELECT o_id FROM orders WHERE o_w_id = 1 AND o_d_id = 1 AND o_c_id = 1 \
             ORDER BY o_id DESC LIMIT 1"
        )?,
        original.order_id
    );
    t1.rollback()?;
    Ok(())
}

#[test]
fn delivery_one_district_does_not_redeliver_a_completed_district() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_district(&mut session, 1, 1)?;
    insert_delivery_order(&mut session, 1, 3001, 1)?;

    let request = DeliveryRequest {
        w_id: 1,
        carrier_id: 7,
        delivery_timestamp_micros: 1,
    };

    let first = delivery_one_district(&mut session, &request, 1)?;
    assert_eq!(
        first,
        DeliveryDistrictResult::Delivered {
            order_id: 3001,
            customer_id: 1,
            sum_amount: 1_000,
        }
    );

    let second = delivery_one_district(&mut session, &request, 1)?;
    assert_eq!(second, DeliveryDistrictResult::Skipped);

    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_balance FROM customer WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1"
        )?,
        1_000
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_delivery_cnt FROM customer \
             WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1"
        )?,
        1
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT COUNT(*) FROM new_order \
             WHERE no_w_id = 1 AND no_d_id = 1 AND no_o_id = 3001"
        )?,
        0
    );
    Ok(())
}

#[test]
fn two_concurrent_new_orders_same_district() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    seed_new_order_data(&mut seed)?;
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|index| {
            let server = Arc::clone(&server);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut session = server.open_session().unwrap();
                barrier.wait();
                retry_new_order(&mut session, &new_order_request(1, index + 1)).order_id
            })
        })
        .collect();
    let mut ids = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();

    assert_eq!(ids, vec![3001, 3002]);
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT d_next_o_id FROM district WHERE d_w_id = 1 AND d_id = 1"
        )?,
        3003
    );
    Ok(())
}

#[test]
fn concurrent_payments_same_customer() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    insert_warehouse(&mut seed, 1)?;
    insert_district(&mut seed, 1, 1)?;
    insert_customer(&mut seed, 1, 1, 1, "100.00")?;
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = [(100, 1), (250, 2)]
        .into_iter()
        .map(|(amount, sequence)| {
            let server = Arc::clone(&server);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut session = server.open_session().unwrap();
                barrier.wait();
                retry_payment(&mut session, &payment_request(amount, sequence));
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(
        query_i64(&mut seed, "SELECT c_balance FROM customer WHERE c_id = 1")?,
        9_650
    );
    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT c_ytd_payment FROM customer WHERE c_id = 1"
        )?,
        350
    );
    Ok(())
}

#[test]
fn two_concurrent_deliveries_same_warehouse() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    insert_delivery_order(&mut seed, 1, 3001, 1)?;
    insert_delivery_order(&mut seed, 2, 3001, 2)?;
    let initial_balances = [(1, 1), (2, 2)]
        .into_iter()
        .map(|(district_id, customer_id)| {
            query_i64(
                &mut seed,
                &format!(
                    "SELECT c_balance FROM customer \
                     WHERE c_w_id = 1 AND c_d_id = {district_id} AND c_id = {customer_id}"
                ),
            )
            .map(|balance| (district_id, customer_id, balance))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let server = Arc::clone(&server);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut session = server.open_session().unwrap();
                barrier.wait();
                for _ in 0..MAX_RETRY_ATTEMPTS {
                    match delivery(
                        &mut session,
                        &DeliveryRequest {
                            w_id: 1,
                            carrier_id: 7,
                            delivery_timestamp_micros: 1,
                        },
                    ) {
                        Ok(result) => return result,
                        Err(TransactionError::Conflict) => continue,
                        Err(error) => panic!("unexpected Delivery error: {error}"),
                    }
                }
                panic!("Delivery exhausted {MAX_RETRY_ATTEMPTS} attempts due to conflicts");
            })
        })
        .collect();
    for handle in handles {
        let result = handle.join().unwrap();
        assert_eq!(result.per_district.len(), 10);
    }

    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT COUNT(*) FROM order_line WHERE ol_delivery_d = 1"
        )?,
        2
    );
    assert_eq!(query_i64(&mut seed, "SELECT COUNT(*) FROM new_order")?, 0);

    for (district_id, customer_id, initial_balance) in initial_balances {
        // Each seeded order has exactly one order line, so avoid SUM over Decimal.
        let order_total = query_i64(
            &mut seed,
            &format!(
                "SELECT ol_amount FROM order_line \
                 WHERE ol_w_id = 1 AND ol_d_id = {district_id} AND ol_o_id = 3001"
            ),
        )?;

        assert_eq!(
            query_i64(
                &mut seed,
                &format!(
                    "SELECT c_delivery_cnt FROM customer \
                     WHERE c_w_id = 1 AND c_d_id = {district_id} AND c_id = {customer_id}"
                ),
            )?,
            1
        );
        assert_eq!(
            query_i64(
                &mut seed,
                &format!(
                    "SELECT c_balance FROM customer \
                     WHERE c_w_id = 1 AND c_d_id = {district_id} AND c_id = {customer_id}"
                ),
            )?,
            initial_balance + order_total
        );
        assert_eq!(
            query_i64(
                &mut seed,
                &format!(
                    "SELECT o_carrier_id FROM orders \
                     WHERE o_w_id = 1 AND o_d_id = {district_id} AND o_id = 3001"
                ),
            )?,
            7
        );
        assert_eq!(
            query_i64(
                &mut seed,
                &format!(
                    "SELECT COUNT(*) FROM order_line \
                     WHERE ol_w_id = 1 AND ol_d_id = {district_id} AND ol_o_id = 3001 \
                     AND ol_delivery_d = 1"
                ),
            )?,
            1
        );
    }
    Ok(())
}

#[test]
fn concurrent_new_orders_same_stock_decrement() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut seed = server.open_session()?;
    seed_new_order_data(&mut seed)?;
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|index| {
            let server = Arc::clone(&server);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut session = server.open_session().unwrap();
                barrier.wait();
                retry_new_order(&mut session, &new_order_request(2, index + 1));
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(
        query_i64(
            &mut seed,
            "SELECT s_quantity FROM stock WHERE s_w_id = 1 AND s_i_id = 1"
        )?,
        96
    );
    Ok(())
}

#[test]
fn two_inserts_same_absent_pk() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    server.execute("CREATE TABLE isolation_pk (id BIGINT PRIMARY KEY, v INT)")?;
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = [10, 20]
        .into_iter()
        .map(|value| {
            let server = Arc::clone(&server);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut session = server.open_session().unwrap();
                session.begin().unwrap();
                session
                    .execute(&format!(
                        "INSERT INTO isolation_pk (id, v) VALUES (1, {value})"
                    ))
                    .unwrap();
                barrier.wait();
                match session.commit() {
                    Ok(()) => true,
                    Err(HtapError::Conflict(_)) => false,
                    Err(error) => panic!("unexpected commit error: {error}"),
                }
            })
        })
        .collect();
    let committed = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|committed| *committed)
        .count();

    assert_eq!(committed, 1);
    let mut session = server.open_session()?;
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT COUNT(*) FROM isolation_pk WHERE id = 1"
        )?,
        1
    );
    Ok(())
}

#[test]
fn insert_over_committed_key_is_upsert() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    execute(
        &mut session,
        "CREATE TABLE isolation_upsert (id BIGINT PRIMARY KEY, v INT)",
    )?;

    session.begin()?;
    execute(
        &mut session,
        "INSERT INTO isolation_upsert (id, v) VALUES (1, 10)",
    )?;
    session.commit()?;

    session.begin()?;
    execute(
        &mut session,
        "INSERT INTO isolation_upsert (id, v) VALUES (1, 20)",
    )?;
    session.commit()?;

    assert_eq!(
        query_i64(&mut session, "SELECT v FROM isolation_upsert WHERE id = 1")?,
        20
    );
    Ok(())
}
