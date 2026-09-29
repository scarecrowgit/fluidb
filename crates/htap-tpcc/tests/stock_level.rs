use std::error::Error;
use std::sync::Arc;

use htap_server::{LocalServer, Session};
use htap_tpcc::schema::ddl_statements;
use htap_tpcc::transactions::{stock_level, StockLevelRequest, TransactionError};
use tempfile::TempDir;

struct OrderLineData {
    district_id: i64,
    order_id: i64,
    line_number: i64,
    item_id: i64,
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

fn insert_district(
    session: &mut Session,
    district_id: i64,
    next_order_id: i64,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO district \
             (d_id, d_w_id, d_name, d_street_1, d_street_2, d_city, d_state, d_zip, \
              d_tax, d_ytd, d_next_o_id) \
             VALUES ({district_id}, 1, 'District', 'street', 'suite', 'city', 'ST', \
                     '123456789', 0.0000, 0.00, {next_order_id})"
        ),
    )
}

fn insert_stock(
    session: &mut Session,
    warehouse_id: i64,
    item_id: i64,
    quantity: i64,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO stock \
             (s_i_id, s_w_id, s_quantity, s_dist_01, s_dist_02, s_dist_03, s_dist_04, \
              s_dist_05, s_dist_06, s_dist_07, s_dist_08, s_dist_09, s_dist_10, \
              s_ytd, s_order_cnt, s_remote_cnt, s_data) \
             VALUES ({item_id}, {warehouse_id}, {quantity}, 'dist', 'dist', 'dist', \
                     'dist', 'dist', 'dist', 'dist', 'dist', 'dist', 'dist', \
                     0, 0, 0, 'stock data')"
        ),
    )
}

fn insert_order_line(
    session: &mut Session,
    order_line: OrderLineData,
) -> Result<(), Box<dyn Error>> {
    execute(
        session,
        &format!(
            "INSERT INTO order_line \
             (ol_o_id, ol_d_id, ol_w_id, ol_number, ol_i_id, ol_supply_w_id, \
              ol_delivery_d, ol_quantity, ol_amount, ol_dist_info) \
             VALUES ({}, {}, 1, {}, {}, 1, NULL, 1, 1.00, 'district info')",
            order_line.order_id, order_line.district_id, order_line.line_number, order_line.item_id
        ),
    )
}

fn request(threshold: i64) -> StockLevelRequest {
    StockLevelRequest {
        w_id: 1,
        d_id: 1,
        threshold,
    }
}

#[test]
fn counts_distinct_low_stock_items_from_last_twenty_orders() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_district(&mut session, 1, 31)?;
    insert_stock(&mut session, 1, 1, 9)?;
    insert_stock(&mut session, 1, 2, 10)?;
    insert_stock(&mut session, 1, 3, 4)?;
    insert_stock(&mut session, 1, 4, 1)?;

    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 11,
            line_number: 1,
            item_id: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 12,
            line_number: 1,
            item_id: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 20,
            line_number: 1,
            item_id: 2,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 30,
            line_number: 1,
            item_id: 3,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 10,
            line_number: 1,
            item_id: 4,
        },
    )?;

    let result = stock_level(&mut session, &request(10))?;

    assert_eq!(result.low_stock, 2);
    Ok(())
}

#[test]
fn excludes_other_districts_and_warehouses() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_district(&mut session, 1, 21)?;
    insert_stock(&mut session, 1, 1, 20)?;
    insert_stock(&mut session, 2, 1, 1)?;
    insert_stock(&mut session, 1, 2, 1)?;

    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 20,
            line_number: 1,
            item_id: 1,
        },
    )?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 2,
            order_id: 20,
            line_number: 1,
            item_id: 2,
        },
    )?;

    let result = stock_level(&mut session, &request(10))?;

    assert_eq!(result.low_stock, 0);
    Ok(())
}

#[test]
fn missing_stock_rows_are_reported() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    insert_district(&mut session, 1, 21)?;
    insert_order_line(
        &mut session,
        OrderLineData {
            district_id: 1,
            order_id: 20,
            line_number: 1,
            item_id: 999,
        },
    )?;

    let error = stock_level(&mut session, &request(10))
        .expect_err("missing stock for a recent order line must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message) if message == "stock was not found"
    ));
    Ok(())
}

#[test]
fn invalid_threshold_is_rejected() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let error =
        stock_level(&mut session, &request(0)).expect_err("nonpositive threshold must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message)
            if message == "warehouse, district, and threshold must be positive"
    ));
    Ok(())
}

#[test]
fn missing_district_is_reported() -> Result<(), Box<dyn Error>> {
    let (_directory, server) = setup()?;
    let mut session = server.open_session()?;

    let error = stock_level(&mut session, &request(10)).expect_err("missing district must fail");

    assert!(matches!(
        error,
        TransactionError::InvalidInput(message) if message == "district was not found"
    ));
    Ok(())
}
