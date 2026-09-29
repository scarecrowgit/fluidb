use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{new_order, NewOrderItem, NewOrderRequest, TransactionError};
use tempfile::TempDir;

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
        panic!("expected query result");
    };
    let value = result.rows()[0].get(0).expect("result value");
    match value {
        Value::Int32(value) => Ok(i64::from(*value)),
        Value::Int64(value) => Ok(*value),
        Value::Decimal { value, .. } => Ok(*value),
        Value::Timestamp(value) => Ok(*value),
        other => panic!("expected integer-compatible value, got {other:?}"),
    }
}

fn insert_warehouse(session: &mut Session, w_id: i64, tax: &str) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO warehouse \
             (w_id, w_name, w_street_1, w_street_2, w_city, w_state, w_zip, w_tax, w_ytd) \
             VALUES ({w_id}, 'W{w_id}', 'street', 'suite', 'city', 'ST', '123456789', {tax}, 0.00)"
        ),
    )
}

fn insert_district(
    session: &mut Session,
    w_id: i64,
    d_id: i64,
    tax: &str,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO district \
             (d_id, d_w_id, d_name, d_street_1, d_street_2, d_city, d_state, d_zip, \
              d_tax, d_ytd, d_next_o_id) \
             VALUES ({d_id}, {w_id}, 'D{d_id}', 'street', 'suite', 'city', 'ST', \
                     '123456789', {tax}, 0.00, 3001)"
        ),
    )
}

fn insert_customer(session: &mut Session) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        "INSERT INTO customer \
         (c_id, c_d_id, c_w_id, c_first, c_middle, c_last, c_street_1, c_street_2, \
          c_city, c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
          c_balance, c_ytd_payment, c_payment_cnt, c_delivery_cnt, c_data) \
         VALUES (1, 1, 1, 'First', 'OE', 'Last', 'street', 'suite', 'city', 'ST', \
                 '123456789', '1234567890123456', DATE '2000-01-01', 'GC', \
                 50000.00, 0.0000, 0.00, 0.00, 0, 0, 'customer data')",
    )
}

fn insert_item(
    session: &mut Session,
    item_id: i64,
    price: &str,
    data: &str,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO item (i_id, i_im_id, i_name, i_price, i_data) \
             VALUES ({item_id}, 1, 'item-{item_id}', {price}, '{data}')"
        ),
    )
}

fn insert_stock(
    session: &mut Session,
    w_id: i64,
    item_id: i64,
    quantity: i64,
    data: &str,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO stock \
             (s_i_id, s_w_id, s_quantity, s_dist_01, s_dist_02, s_dist_03, s_dist_04, \
              s_dist_05, s_dist_06, s_dist_07, s_dist_08, s_dist_09, s_dist_10, \
              s_ytd, s_order_cnt, s_remote_cnt, s_data) \
             VALUES ({item_id}, {w_id}, {quantity}, 'dist', 'dist', 'dist', 'dist', \
                     'dist', 'dist', 'dist', 'dist', 'dist', 'dist', 0, 0, 0, '{data}')"
        ),
    )
}

fn seed_order_data(session: &mut Session) -> Result<(), Box<dyn Error>> {
    insert_warehouse(session, 1, "0.0000")?;
    insert_district(session, 1, 1, "0.0000")?;
    insert_customer(session)?;
    Ok(())
}

fn request(items: Vec<NewOrderItem>, entry_timestamp_micros: i64) -> NewOrderRequest {
    NewOrderRequest {
        w_id: 1,
        d_id: 1,
        c_id: 1,
        items,
        entry_timestamp_micros,
    }
}

#[test]
fn all_home_lines() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    seed_order_data(&mut session)?;

    let mut items = Vec::new();
    for item_id in 1..=5 {
        insert_item(&mut session, item_id, "10.00", "data")?;
        insert_stock(&mut session, 1, item_id, 20, "data")?;
        items.push(NewOrderItem {
            item_id,
            supply_w_id: 1,
            quantity: 1,
        });
    }

    let expected_timestamp = 1_714_998_855_123_456;
    let result = new_order(&mut session, &request(items, expected_timestamp))?;
    let entry_timestamp_micros = result.entry_timestamp_micros;

    assert_eq!(entry_timestamp_micros, expected_timestamp);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_entry_d FROM orders WHERE o_id = 3001"
        )?,
        expected_timestamp
    );
    assert!(result.all_local);
    assert_eq!(
        query_i64(&mut session, "SELECT o_all_local FROM orders")?,
        1
    );
    assert_eq!(
        query_i64(&mut session, "SELECT COUNT(*) FROM order_line")?,
        5
    );
    for (item_id, line) in (1..=5).zip(&result.order_lines) {
        assert_eq!(line.item_name, format!("item-{item_id}"));
    }
    Ok(())
}

#[test]
fn one_step_total_rounding() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1, "0.3333")?;
    insert_district(&mut session, 1, 1, "0.0000")?;
    insert_customer(&mut session)?;
    execute(
        &mut session,
        "UPDATE customer SET c_discount = 0.3333 WHERE c_w_id = 1 AND c_d_id = 1 AND c_id = 1",
    )?;

    let mut items = Vec::new();
    for item_id in 1..=5 {
        insert_item(&mut session, item_id, "0.01", "data")?;
        insert_stock(&mut session, 1, item_id, 20, "data")?;
        items.push(NewOrderItem {
            item_id,
            supply_w_id: 1,
            quantity: 1,
        });
    }

    let result = new_order(&mut session, &request(items, 1_715_003_445_654_320))?;

    // 5 * 0.6667 * 1.3333 = 4.444... cents, which rounds once to 4 cents.
    assert_eq!(result.total_amount, 4);
    Ok(())
}

#[test]
fn remote_lines() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    seed_order_data(&mut session)?;
    insert_warehouse(&mut session, 2, "0.0000")?;

    let mut items = Vec::new();
    for item_id in 1..=5 {
        insert_item(&mut session, item_id, "10.00", "data")?;
        let supply_w_id = if item_id == 5 { 2 } else { 1 };
        insert_stock(&mut session, supply_w_id, item_id, 20, "data")?;
        items.push(NewOrderItem {
            item_id,
            supply_w_id,
            quantity: 1,
        });
    }

    let expected_timestamp = 1_715_003_445_654_321;
    let result = new_order(&mut session, &request(items, expected_timestamp))?;
    let entry_timestamp_micros = result.entry_timestamp_micros;

    assert_eq!(entry_timestamp_micros, expected_timestamp);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_entry_d FROM orders WHERE o_id = 3001"
        )?,
        expected_timestamp
    );
    assert!(!result.all_local);
    assert_eq!(
        query_i64(&mut session, "SELECT o_all_local FROM orders")?,
        0
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT s_remote_cnt FROM stock WHERE s_w_id = 2 AND s_i_id = 5"
        )?,
        1
    );
    Ok(())
}

#[test]
fn stock_below_threshold() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    seed_order_data(&mut session)?;

    let mut items = Vec::new();
    for item_id in 1..=5 {
        insert_item(&mut session, item_id, "10.00", "data")?;
        insert_stock(
            &mut session,
            1,
            item_id,
            if item_id == 1 { 5 } else { 20 },
            "data",
        )?;
        items.push(NewOrderItem {
            item_id,
            supply_w_id: 1,
            quantity: if item_id == 1 { 3 } else { 1 },
        });
    }

    let expected_timestamp = 1_715_054_130_111_111;
    let result = new_order(&mut session, &request(items, expected_timestamp))?;
    let entry_timestamp_micros = result.entry_timestamp_micros;

    assert_eq!(entry_timestamp_micros, expected_timestamp);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_entry_d FROM orders WHERE o_id = 3001"
        )?,
        expected_timestamp
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT s_quantity FROM stock WHERE s_w_id = 1 AND s_i_id = 1"
        )?,
        93
    );
    Ok(())
}

#[test]
fn brand_generic() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    seed_order_data(&mut session)?;

    for item_id in 1..=5 {
        let item_data = if item_id == 1 {
            "ORIGINAL item"
        } else {
            "item"
        };
        let stock_data = if item_id == 1 {
            "ORIGINAL stock"
        } else {
            "stock"
        };
        insert_item(&mut session, item_id, "10.00", item_data)?;
        insert_stock(&mut session, 1, item_id, 20, stock_data)?;
    }

    let result = new_order(
        &mut session,
        &request(
            (1..=5)
                .map(|item_id| NewOrderItem {
                    item_id,
                    supply_w_id: 1,
                    quantity: 1,
                })
                .collect(),
            1_715_145_930_222_222,
        ),
    )?;
    let expected_timestamp = 1_715_145_930_222_222;
    let entry_timestamp_micros = result.entry_timestamp_micros;

    assert_eq!(entry_timestamp_micros, expected_timestamp);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_entry_d FROM orders WHERE o_id = 3001"
        )?,
        expected_timestamp
    );
    assert_eq!(result.order_lines[0].brand_generic, 'B');
    assert!(result.order_lines[1..]
        .iter()
        .all(|line| line.brand_generic == 'G'));
    assert_eq!(
        query_i64(&mut session, "SELECT COUNT(*) FROM order_line")?,
        5
    );
    Ok(())
}

#[test]
fn unused_item_rollback() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    seed_order_data(&mut session)?;

    for item_id in 1..=4 {
        insert_item(&mut session, item_id, "10.00", "data")?;
        insert_stock(&mut session, 1, item_id, 20, "data")?;
    }

    let error = new_order(
        &mut session,
        &request(
            vec![
                NewOrderItem {
                    item_id: 1,
                    supply_w_id: 1,
                    quantity: 1,
                },
                NewOrderItem {
                    item_id: 2,
                    supply_w_id: 1,
                    quantity: 1,
                },
                NewOrderItem {
                    item_id: 3,
                    supply_w_id: 1,
                    quantity: 1,
                },
                NewOrderItem {
                    item_id: 4,
                    supply_w_id: 1,
                    quantity: 1,
                },
                NewOrderItem {
                    item_id: 999,
                    supply_w_id: 1,
                    quantity: 1,
                },
            ],
            1_715_237_730_333_333,
        ),
    )
    .expect_err("missing item must roll back the transaction");

    assert!(matches!(error, TransactionError::ExpectedRollback));
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT d_next_o_id FROM district WHERE d_w_id = 1 AND d_id = 1"
        )?,
        3001
    );
    assert_eq!(query_i64(&mut session, "SELECT COUNT(*) FROM orders")?, 0);
    assert_eq!(
        query_i64(&mut session, "SELECT COUNT(*) FROM new_order")?,
        0
    );
    assert_eq!(
        query_i64(&mut session, "SELECT COUNT(*) FROM order_line")?,
        0
    );
    Ok(())
}

#[test]
fn exact_amount() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1, "0.0500")?;
    insert_district(&mut session, 1, 1, "0.0100")?;
    insert_customer(&mut session)?;

    let mut items = Vec::new();
    for item_id in 1..=5 {
        insert_item(&mut session, item_id, "25.00", "data")?;
        insert_stock(&mut session, 1, item_id, 20, "data")?;
        items.push(NewOrderItem {
            item_id,
            supply_w_id: 1,
            quantity: 2,
        });
    }

    let expected_timestamp = 1_715_329_530_444_444;
    let result = new_order(&mut session, &request(items, expected_timestamp))?;
    let entry_timestamp_micros = result.entry_timestamp_micros;

    assert_eq!(entry_timestamp_micros, expected_timestamp);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_entry_d FROM orders WHERE o_id = 3001"
        )?,
        expected_timestamp
    );
    assert_eq!(result.total_amount, 26_500);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT ol_amount FROM order_line WHERE ol_number = 1"
        )?,
        5_000
    );
    Ok(())
}

#[test]
fn both_stock_rules() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    seed_order_data(&mut session)?;

    for item_id in 1..=5 {
        insert_item(&mut session, item_id, "10.00", "data")?;
        insert_stock(
            &mut session,
            1,
            item_id,
            if item_id == 1 {
                20
            } else if item_id == 2 {
                5
            } else {
                20
            },
            "data",
        )?;
    }

    let result = new_order(
        &mut session,
        &request(
            vec![
                NewOrderItem {
                    item_id: 1,
                    supply_w_id: 1,
                    quantity: 3,
                },
                NewOrderItem {
                    item_id: 2,
                    supply_w_id: 1,
                    quantity: 3,
                },
                NewOrderItem {
                    item_id: 3,
                    supply_w_id: 1,
                    quantity: 1,
                },
                NewOrderItem {
                    item_id: 4,
                    supply_w_id: 1,
                    quantity: 1,
                },
                NewOrderItem {
                    item_id: 5,
                    supply_w_id: 1,
                    quantity: 1,
                },
            ],
            1_715_421_330_555_555,
        ),
    )?;
    let expected_timestamp = 1_715_421_330_555_555;
    let entry_timestamp_micros = result.entry_timestamp_micros;

    assert_eq!(entry_timestamp_micros, expected_timestamp);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_entry_d FROM orders WHERE o_id = 3001"
        )?,
        expected_timestamp
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT s_quantity FROM stock WHERE s_w_id = 1 AND s_i_id = 1"
        )?,
        17
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT s_quantity FROM stock WHERE s_w_id = 1 AND s_i_id = 2"
        )?,
        93
    );
    Ok(())
}
