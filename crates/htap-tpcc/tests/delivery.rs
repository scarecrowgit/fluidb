use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{
    delivery, DeliveryDistrictResult, DeliveryRequest, TransactionError,
};
use tempfile::TempDir;

struct OrderData {
    district_id: i64,
    order_id: i64,
    customer_id: i64,
    line_count: i64,
}

struct OrderLineData<'a> {
    district_id: i64,
    order_id: i64,
    line_number: i64,
    amount: &'a str,
}

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

fn insert_customer(
    session: &mut Session,
    district_id: i64,
    customer_id: i64,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO customer \
             (c_id, c_d_id, c_w_id, c_first, c_middle, c_last, c_street_1, c_street_2, \
              c_city, c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
              c_balance, c_ytd_payment, c_payment_cnt, c_delivery_cnt, c_data) \
             VALUES ({customer_id}, {district_id}, 1, 'First', 'OE', 'Last', \
                     'street', 'suite', 'city', 'ST', '123456789', '1234567890123456', \
                     DATE '2000-01-01', 'GC', 50000.00, 0.0000, 0.00, 0.00, 0, 0, \
                     'customer data')"
        ),
    )
}

fn insert_order(session: &mut Session, order: OrderData) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO orders \
             (o_id, o_d_id, o_w_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) \
             VALUES ({}, {}, 1, {}, 1715500000000000, NULL, {}, 1)",
            order.order_id, order.district_id, order.customer_id, order.line_count
        ),
    )?;
    execute(
        session,
        &format!(
            "INSERT INTO new_order (no_o_id, no_d_id, no_w_id) \
             VALUES ({}, {}, 1)",
            order.order_id, order.district_id
        ),
    )
}

fn insert_order_line(
    session: &mut Session,
    order_line: OrderLineData<'_>,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO order_line \
             (ol_o_id, ol_d_id, ol_w_id, ol_number, ol_i_id, ol_supply_w_id, \
              ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) \
             VALUES ({}, {}, 1, {}, {}, 1, NULL, 1, {}, 'district info')",
            order_line.order_id,
            order_line.district_id,
            order_line.line_number,
            order_line.line_number,
            order_line.amount
        ),
    )
}

fn request() -> DeliveryRequest {
    DeliveryRequest {
        w_id: 1,
        carrier_id: 7,
        delivery_timestamp_micros: 1_715_600_000_123_456,
    }
}

#[test]
fn delivers_oldest_order_in_each_district() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_customer(&mut session, 1, 1)?;
    insert_order(
        &mut session,
        OrderData {
            district_id: 1,
            order_id: 3002,
            customer_id: 1,
            line_count: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 3002,
            line_number: 1,
            amount: "30.00",
        },
    )?;
    insert_order(
        &mut session,
        OrderData {
            district_id: 1,
            order_id: 3001,
            customer_id: 1,
            line_count: 2,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 3001,
            line_number: 1,
            amount: "5.00",
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 3001,
            line_number: 2,
            amount: "7.50",
        },
    )?;

    insert_customer(&mut session, 2, 2)?;
    insert_order(
        &mut session,
        OrderData {
            district_id: 2,
            order_id: 4001,
            customer_id: 2,
            line_count: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 2,
            order_id: 4001,
            line_number: 1,
            amount: "9.25",
        },
    )?;

    let result = delivery(&mut session, &request())?;

    assert_eq!(result.per_district.len(), 10);
    assert_eq!(
        result.per_district[0],
        DeliveryDistrictResult::Delivered {
            order_id: 3001,
            customer_id: 1,
            sum_amount: 1_250,
        }
    );
    assert_eq!(
        result.per_district[1],
        DeliveryDistrictResult::Delivered {
            order_id: 4001,
            customer_id: 2,
            sum_amount: 925,
        }
    );
    assert!(result.per_district[2..]
        .iter()
        .all(|district| *district == DeliveryDistrictResult::Skipped));

    assert_eq!(
        query_i64(
            &mut session,
            "SELECT COUNT(*) FROM new_order WHERE no_d_id = 1"
        )?,
        1
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT no_o_id FROM new_order WHERE no_d_id = 1"
        )?,
        3002
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT o_carrier_id FROM orders WHERE o_d_id = 1 AND o_id = 3001"
        )?,
        7
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT ol_delivery_d FROM order_line \
             WHERE ol_d_id = 1 AND ol_o_id = 3001 AND ol_number = 1"
        )?,
        1_715_600_000_123_456
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_balance FROM customer WHERE c_d_id = 1 AND c_id = 1"
        )?,
        1_250
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_delivery_cnt FROM customer WHERE c_d_id = 1 AND c_id = 1"
        )?,
        1
    );
    Ok(())
}

#[test]
fn skips_districts_without_outstanding_orders() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let result = delivery(&mut session, &request())?;

    assert_eq!(
        result.per_district,
        vec![DeliveryDistrictResult::Skipped; 10]
    );
    Ok(())
}

#[test]
fn missing_order_lines_rolls_back_district() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_customer(&mut session, 1, 1)?;
    insert_order(
        &mut session,
        OrderData {
            district_id: 1,
            order_id: 3001,
            customer_id: 1,
            line_count: 1,
        },
    )?;

    let error =
        delivery(&mut session, &request()).expect_err("an order without order lines must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message) if message == "order lines were not found"
    ));
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT COUNT(*) FROM new_order WHERE no_d_id = 1 AND no_o_id = 3001"
        )?,
        1
    );
    Ok(())
}

#[test]
fn invalid_request_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let error = delivery(
        &mut session,
        &DeliveryRequest {
            w_id: 1,
            carrier_id: 0,
            delivery_timestamp_micros: 1_715_600_000_123_456,
        },
    )
    .expect_err("nonpositive carrier ID must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message)
            if message == "warehouse and carrier IDs must be positive"
    ));
    Ok(())
}
