use std::error::Error;
use std::sync::Arc;

use htap_common::types::Value;
use htap_server::{LocalServer, Session};
use htap_sql::result::StatementResult;
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{payment, CustomerSelector, PaymentRequest};
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
    match result.rows()[0].get(0).expect("result value") {
        Value::Int32(value) => Ok(i64::from(*value)),
        Value::Int64(value) => Ok(*value),
        Value::Decimal { value, .. } => Ok(*value),
        Value::Timestamp(value) => Ok(*value),
        other => panic!("expected integer-compatible value, got {other:?}"),
    }
}

fn query_string(session: &mut Session, sql: &str) -> Result<String, Box<dyn Error>> {
    let StatementResult::Query(result) = session.execute(sql)? else {
        panic!("expected query result");
    };
    match result.rows()[0].get(0).expect("result value") {
        Value::String(value) => Ok(value.clone()),
        other => panic!("expected string value, got {other:?}"),
    }
}

fn insert_warehouse(session: &mut Session, w_id: i64) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO warehouse \
             (w_id, w_name, w_street_1, w_street_2, w_city, w_state, w_zip, w_tax, w_ytd) \
             VALUES ({w_id}, 'Warehouse {w_id}', 'warehouse street 1', 'warehouse street 2', \
                     'warehouse city', 'WS', '111112222', 0.0000, 0.00)"
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
             VALUES ({d_id}, {w_id}, 'District {d_id}', 'district street 1', \
                     'district street 2', 'district city', 'DS', '333334444', \
                     0.0000, 0.00, 3001)"
        ),
    )
}

struct CustomerData<'a> {
    c_id: i64,
    c_w_id: i64,
    c_d_id: i64,
    first: &'a str,
    last: &'a str,
    credit: &'a str,
    data: &'a str,
}

fn insert_customer(
    session: &mut Session,
    customer: &CustomerData<'_>,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO customer \
             (c_id, c_d_id, c_w_id, c_first, c_middle, c_last, c_street_1, c_street_2, \
              c_city, c_state, c_zip, c_phone, c_since, c_credit, c_credit_lim, c_discount, \
              c_balance, c_ytd_payment, c_payment_cnt, c_delivery_cnt, c_data) \
             VALUES ({}, {}, {}, '{}', 'OE', '{}', 'customer street 1', \
                     'customer street 2', 'customer city', 'CS', '555556666', \
                     '9876543210987654', \
                     DATE '2000-01-01', \
                     '{}', 50000.00, 0.0000, 100.00, 10.00, 1, 0, '{}')",
            customer.c_id,
            customer.c_d_id,
            customer.c_w_id,
            customer.first,
            customer.last,
            customer.credit,
            customer.data,
        ),
    )
}

fn request(
    customer: CustomerSelector,
    sequence: u64,
    entry_timestamp_micros: i64,
) -> PaymentRequest {
    PaymentRequest {
        w_id: 1,
        d_id: 1,
        customer_w_id: 1,
        customer_d_id: 1,
        customer,
        h_amount: 1_234,
        h_terminal: 1,
        h_sequence: sequence,
        entry_timestamp_micros,
    }
}

#[test]
fn by_id_good_credit() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_district(&mut session, 1, 1)?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 1,
            c_w_id: 1,
            c_d_id: 1,
            first: "Alice",
            last: "Good",
            credit: "GC",
            data: "unchanged",
        },
    )?;

    let result = payment(
        &mut session,
        &request(CustomerSelector::Id(1), 1, 1_715_513_130_666_666),
    )?;

    assert_eq!(result.customer_id, 1);
    assert_eq!(result.warehouse_name, "Warehouse 1");
    assert_eq!(result.warehouse_street_1, "warehouse street 1");
    assert_eq!(result.warehouse_street_2, "warehouse street 2");
    assert_eq!(result.warehouse_city, "warehouse city");
    assert_eq!(result.warehouse_state, "WS");
    assert_eq!(result.warehouse_zip, "111112222");
    assert_eq!(result.district_name, "District 1");
    assert_eq!(result.district_street_1, "district street 1");
    assert_eq!(result.district_street_2, "district street 2");
    assert_eq!(result.district_city, "district city");
    assert_eq!(result.district_state, "DS");
    assert_eq!(result.district_zip, "333334444");
    assert_eq!(result.customer_street_1, "customer street 1");
    assert_eq!(result.customer_street_2, "customer street 2");
    assert_eq!(result.customer_city, "customer city");
    assert_eq!(result.customer_state, "CS");
    assert_eq!(result.customer_zip, "555556666");
    assert_eq!(result.customer_phone, "9876543210987654");
    assert_eq!(result.customer_since_micros, 946_684_800_000_000);
    assert_eq!(result.customer_credit, "GC");
    assert_eq!(result.customer_credit_limit, 5_000_000);
    assert_eq!(result.customer_discount, 0);
    assert_eq!(result.customer_balance, 8_766);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_balance FROM customer WHERE c_id = 1"
        )?,
        8_766
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_payment_cnt FROM customer WHERE c_id = 1"
        )?,
        2
    );
    assert_eq!(
        query_string(&mut session, "SELECT c_data FROM customer WHERE c_id = 1")?,
        "unchanged"
    );
    assert_eq!(query_i64(&mut session, "SELECT COUNT(*) FROM history")?, 1);
    Ok(())
}

#[test]
fn by_id_bad_credit() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_district(&mut session, 1, 1)?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 1,
            c_w_id: 1,
            c_d_id: 1,
            first: "Bob",
            last: "Bad",
            credit: "BC",
            data: "old data",
        },
    )?;

    let result = payment(
        &mut session,
        &request(CustomerSelector::Id(1), 1, 1_715_604_930_777_777),
    )?;

    assert_eq!(result.customer_credit, "BC");
    assert_eq!(
        query_string(&mut session, "SELECT c_data FROM customer WHERE c_id = 1")?,
        "1 1 1 1 1 12.34|old data"
    );
    assert_eq!(
        query_i64(&mut session, "SELECT h_date FROM history")?,
        1_715_604_930_777_777
    );
    assert_eq!(query_i64(&mut session, "SELECT COUNT(*) FROM history")?, 1);
    Ok(())
}

#[test]
fn by_last_odd_count() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_district(&mut session, 1, 1)?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 1,
            c_w_id: 1,
            c_d_id: 1,
            first: "Alice",
            last: "Smith",
            credit: "GC",
            data: "data",
        },
    )?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 2,
            c_w_id: 1,
            c_d_id: 1,
            first: "Bob",
            last: "Smith",
            credit: "GC",
            data: "data",
        },
    )?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 3,
            c_w_id: 1,
            c_d_id: 1,
            first: "Carol",
            last: "Smith",
            credit: "GC",
            data: "data",
        },
    )?;

    let result = payment(
        &mut session,
        &request(
            CustomerSelector::LastName("Smith".into()),
            1,
            1_715_696_730_888_888,
        ),
    )?;

    assert_eq!(result.customer_id, 2);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_payment_cnt FROM customer WHERE c_id = 2"
        )?,
        2
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_payment_cnt FROM customer WHERE c_id = 1"
        )?,
        1
    );
    assert_eq!(
        query_i64(&mut session, "SELECT h_date FROM history")?,
        1_715_696_730_888_888
    );
    Ok(())
}

#[test]
fn by_last_even_count() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_district(&mut session, 1, 1)?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 1,
            c_w_id: 1,
            c_d_id: 1,
            first: "Alice",
            last: "Jones",
            credit: "GC",
            data: "data",
        },
    )?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 2,
            c_w_id: 1,
            c_d_id: 1,
            first: "Bob",
            last: "Jones",
            credit: "GC",
            data: "data",
        },
    )?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 3,
            c_w_id: 1,
            c_d_id: 1,
            first: "Carol",
            last: "Jones",
            credit: "GC",
            data: "data",
        },
    )?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 4,
            c_w_id: 1,
            c_d_id: 1,
            first: "David",
            last: "Jones",
            credit: "GC",
            data: "data",
        },
    )?;

    let result = payment(
        &mut session,
        &request(
            CustomerSelector::LastName("Jones".into()),
            1,
            1_715_788_530_999_999,
        ),
    )?;

    assert_eq!(result.customer_id, 2);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_payment_cnt FROM customer WHERE c_id = 2"
        )?,
        2
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_payment_cnt FROM customer WHERE c_id = 3"
        )?,
        1
    );
    assert_eq!(
        query_i64(&mut session, "SELECT h_date FROM history")?,
        1_715_788_530_999_999
    );
    Ok(())
}

#[test]
fn remote_customer() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_warehouse(&mut session, 2)?;
    insert_district(&mut session, 1, 1)?;
    insert_district(&mut session, 2, 1)?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 1,
            c_w_id: 2,
            c_d_id: 1,
            first: "Remote",
            last: "Customer",
            credit: "GC",
            data: "data",
        },
    )?;

    let mut payment_request = request(CustomerSelector::Id(1), 1, 1_715_880_331_111_111);
    payment_request.customer_w_id = 2;
    let result = payment(&mut session, &payment_request)?;

    assert_eq!(result.customer_id, 1);
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_balance FROM customer WHERE c_w_id = 2 AND c_d_id = 1 AND c_id = 1"
        )?,
        8_766
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT h_c_w_id FROM history WHERE h_id = 281474976710657"
        )?,
        2
    );
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT h_date FROM history WHERE h_id = 281474976710657"
        )?,
        1_715_880_331_111_111
    );
    Ok(())
}

#[test]
fn c_data_truncation() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;
    insert_warehouse(&mut session, 1)?;
    insert_district(&mut session, 1, 1)?;
    insert_customer(
        &mut session,
        &CustomerData {
            c_id: 1,
            c_w_id: 1,
            c_d_id: 1,
            first: "Truncate",
            last: "Customer",
            credit: "BC",
            data: &"x".repeat(490),
        },
    )?;

    let timestamps = [
        1_715_972_131_222_222,
        1_715_972_132_333_333,
        1_715_972_133_444_444,
    ];
    let first_history_id = payment(
        &mut session,
        &request(CustomerSelector::Id(1), 1, timestamps[0]),
    )?
    .history_id;
    let second_history_id = payment(
        &mut session,
        &request(CustomerSelector::Id(1), 2, timestamps[1]),
    )?
    .history_id;
    let third_history_id = payment(
        &mut session,
        &request(CustomerSelector::Id(1), 3, timestamps[2]),
    )?
    .history_id;

    let data = query_string(&mut session, "SELECT c_data FROM customer WHERE c_id = 1")?;
    assert_eq!(data.len(), 500);
    assert!(data.starts_with("1 1 1 1 1 12.34|"));
    assert_eq!(
        query_i64(
            &mut session,
            "SELECT c_payment_cnt FROM customer WHERE c_id = 1"
        )?,
        4
    );
    assert_eq!(
        query_i64(
            &mut session,
            &format!("SELECT h_date FROM history WHERE h_id = {first_history_id}")
        )?,
        1_715_972_131_222_222
    );
    assert_eq!(
        query_i64(
            &mut session,
            &format!("SELECT h_date FROM history WHERE h_id = {second_history_id}")
        )?,
        1_715_972_132_333_333
    );
    assert_eq!(
        query_i64(
            &mut session,
            &format!("SELECT h_date FROM history WHERE h_id = {third_history_id}")
        )?,
        1_715_972_133_444_444
    );
    assert_eq!(query_i64(&mut session, "SELECT COUNT(*) FROM history")?, 3);
    Ok(())
}
