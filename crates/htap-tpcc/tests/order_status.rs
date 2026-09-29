use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{
    order_status, CustomerSelector, OrderStatusRequest, TransactionError,
};
use tempfile::TempDir;

struct CustomerData<'a> {
    customer_id: i64,
    first: &'a str,
    last: &'a str,
    balance: &'a str,
}

struct OrderData {
    order_id: i64,
    customer_id: i64,
    entry_timestamp_micros: i64,
    carrier_id: Option<i64>,
    line_count: i64,
}

struct OrderLineData<'a> {
    order_id: i64,
    line_number: i64,
    item_id: i64,
    supply_w_id: i64,
    quantity: i64,
    amount: &'a str,
    delivery_timestamp_micros: Option<i64>,
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
    customer: CustomerData<'_>,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO customer \
             (c_id, c_d_id, c_w_id, c_first, c_middle, c_last, c_street_1, c_street_2, \
              c_city, c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
              c_balance, c_ytd_payment, c_payment_cnt, c_delivery_cnt, c_data) \
             VALUES ({}, 1, 1, '{}', 'OE', '{}', 'street', 'suite', \
                     'city', 'ST', '123456789', '1234567890123456', DATE '2000-01-01', \
                     'GC', 50000.00, 0.0000, {}, 0.00, 0, 0, 'customer data')",
            customer.customer_id, customer.first, customer.last, customer.balance
        ),
    )
}

fn insert_order(session: &mut Session, order: OrderData) -> Result<(), Box<dyn Error>> {
    let carrier_id = order
        .carrier_id
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".into());
    execute(
        session,
        &format!(
            "INSERT INTO orders \
             (o_id, o_d_id, o_w_id, o_c_id, o_entry_d, o_carrier_id, o_ol_cnt, o_all_local) \
             VALUES ({}, 1, 1, {}, {}, {}, {}, 1)",
            order.order_id,
            order.customer_id,
            order.entry_timestamp_micros,
            carrier_id,
            order.line_count
        ),
    )
}

fn insert_order_line(
    session: &mut Session,
    order_line: OrderLineData<'_>,
) -> Result<(), Box<dyn Error>> {
    let delivery_timestamp_micros = order_line
        .delivery_timestamp_micros
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".into());
    execute(
        session,
        &format!(
            "INSERT INTO order_line \
             (ol_o_id, ol_d_id, ol_w_id, ol_number, ol_i_id, ol_supply_w_id, \
              ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) \
             VALUES ({}, 1, 1, {}, {}, {}, {}, {}, {}, 'district info')",
            order_line.order_id,
            order_line.line_number,
            order_line.item_id,
            order_line.supply_w_id,
            delivery_timestamp_micros,
            order_line.quantity,
            order_line.amount
        ),
    )
}

fn request(customer: CustomerSelector) -> OrderStatusRequest {
    OrderStatusRequest {
        w_id: 1,
        d_id: 1,
        customer,
        entry_timestamp_micros: 1_715_500_000_000_000,
    }
}

#[test]
fn selects_most_recent_order() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_customer(
        &mut session,
        CustomerData {
            customer_id: 1,
            first: "First",
            last: "Last",
            balance: "12.34",
        },
    )?;
    insert_order(
        &mut session,
        OrderData {
            order_id: 3001,
            customer_id: 1,
            entry_timestamp_micros: 1_715_500_001_000_000,
            carrier_id: Some(2),
            line_count: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            order_id: 3001,
            line_number: 1,
            item_id: 101,
            supply_w_id: 1,
            quantity: 2,
            amount: "20.00",
            delivery_timestamp_micros: Some(1_715_500_002_000_000),
        },
    )?;
    insert_order(
        &mut session,
        OrderData {
            order_id: 3002,
            customer_id: 1,
            entry_timestamp_micros: 1_715_500_003_000_000,
            carrier_id: None,
            line_count: 2,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            order_id: 3002,
            line_number: 1,
            item_id: 102,
            supply_w_id: 1,
            quantity: 3,
            amount: "30.00",
            delivery_timestamp_micros: None,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            order_id: 3002,
            line_number: 2,
            item_id: 103,
            supply_w_id: 2,
            quantity: 4,
            amount: "40.00",
            delivery_timestamp_micros: None,
        },
    )?;

    let result = order_status(&mut session, &request(CustomerSelector::Id(1)))?;

    assert_eq!(result.customer_id, 1);
    assert_eq!(result.customer_first, "First");
    assert_eq!(result.customer_middle, "OE");
    assert_eq!(result.customer_last, "Last");
    assert_eq!(result.customer_balance, 1_234);
    assert_eq!(result.order_id, 3002);
    assert_eq!(result.order_entry_timestamp_micros, 1_715_500_003_000_000);
    assert_eq!(result.order_carrier_id, None);
    assert_eq!(result.order_lines.len(), 2);
    assert_eq!(result.order_lines[0].item_id, 102);
    assert_eq!(result.order_lines[0].quantity, 3);
    assert_eq!(result.order_lines[0].amount, 3_000);
    assert_eq!(result.order_lines[0].delivery_d, None);
    assert_eq!(result.order_lines[1].item_id, 103);
    assert_eq!(result.order_lines[1].supply_w_id, 2);
    assert_eq!(
        query_i64(&mut session, "SELECT COUNT(*) FROM orders WHERE o_c_id = 1")?,
        2
    );
    Ok(())
}

#[test]
fn last_name_selects_lower_median_customer() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_customer(
        &mut session,
        CustomerData {
            customer_id: 1,
            first: "Charlie",
            last: "Smith",
            balance: "1.00",
        },
    )?;
    insert_customer(
        &mut session,
        CustomerData {
            customer_id: 2,
            first: "Alice",
            last: "Smith",
            balance: "2.00",
        },
    )?;
    insert_customer(
        &mut session,
        CustomerData {
            customer_id: 3,
            first: "Bob",
            last: "Smith",
            balance: "3.00",
        },
    )?;
    insert_customer(
        &mut session,
        CustomerData {
            customer_id: 4,
            first: "Delta",
            last: "Smith",
            balance: "4.00",
        },
    )?;
    insert_order(
        &mut session,
        OrderData {
            order_id: 3001,
            customer_id: 3,
            entry_timestamp_micros: 1_715_500_010_000_000,
            carrier_id: Some(7),
            line_count: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            order_id: 3001,
            line_number: 1,
            item_id: 201,
            supply_w_id: 1,
            quantity: 5,
            amount: "50.00",
            delivery_timestamp_micros: Some(1_715_500_011_000_000),
        },
    )?;

    let result = order_status(
        &mut session,
        &request(CustomerSelector::LastName("Smith".into())),
    )?;

    assert_eq!(result.customer_id, 3);
    assert_eq!(result.customer_first, "Bob");
    assert_eq!(result.customer_balance, 300);
    assert_eq!(result.order_id, 3001);
    assert_eq!(result.order_carrier_id, Some(7));
    assert_eq!(
        result.order_lines[0].delivery_d,
        Some(1_715_500_011_000_000)
    );
    Ok(())
}

#[test]
fn missing_customer_rolls_back() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let error = order_status(&mut session, &request(CustomerSelector::Id(999)))
        .expect_err("missing customer must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message) if message == "customer was not found"
    ));
    Ok(())
}

#[test]
fn invalid_customer_selector_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let error = order_status(
        &mut session,
        &request(CustomerSelector::LastName(String::new())),
    )
    .expect_err("empty customer last name must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message)
            if message == "customer last name must not be empty"
    ));
    Ok(())
}
