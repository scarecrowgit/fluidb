use htap_common::types::{Row, Value};
use htap_server::Session;
use htap_sql::result::{QueryResult, StatementResult};

use crate::transactions::{Result, TransactionError};

/// A TPC-C database consistency condition that was not satisfied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyViolation {
    pub condition_number: i64,
    pub warehouse_id: Option<i64>,
    pub district_id: Option<i64>,
    pub details: String,
}

fn query(session: &mut Session, sql: &str) -> Result<QueryResult> {
    match session.execute(sql).map_err(TransactionError::from)? {
        StatementResult::Query(result) => Ok(result),
        result => Err(TransactionError::InvalidInput(format!(
            "expected query result for '{sql}', got {result:?}"
        ))),
    }
}

fn int(row: &Row, index: usize) -> Result<i64> {
    match row.get(index) {
        Some(&Value::Int32(value)) => Ok(i64::from(value)),
        Some(&Value::Int64(value)) => Ok(value),
        Some(&Value::Decimal { value, .. }) => Ok(value),
        value => Err(TransactionError::InvalidInput(format!(
            "expected integer-compatible result column {index}, got {value:?}"
        ))),
    }
}

fn optional_int(row: &Row, index: usize) -> Result<Option<i64>> {
    match row.get(index) {
        Some(Value::Null) => Ok(None),
        Some(&Value::Int32(value)) => Ok(Some(i64::from(value))),
        Some(&Value::Int64(value)) => Ok(Some(value)),
        Some(&Value::Decimal { value, .. }) => Ok(Some(value)),
        Some(&Value::Timestamp(value)) => Ok(Some(value)),
        value => Err(TransactionError::InvalidInput(format!(
            "expected nullable integer-compatible result column {index}, got {value:?}"
        ))),
    }
}

fn violation(
    condition_number: i64,
    warehouse_id: i64,
    district_id: Option<i64>,
    details: impl Into<String>,
) -> ConsistencyViolation {
    ConsistencyViolation {
        condition_number,
        warehouse_id: Some(warehouse_id),
        district_id,
        details: details.into(),
    }
}

fn warehouse_filter(warehouse_ids: &[i64], column: &str) -> String {
    warehouse_ids
        .iter()
        .map(|warehouse_id| format!("{column} = {warehouse_id}"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Checks the twelve TPC-C consistency conditions for the selected warehouses.
///
/// Returns `InvalidInput` when a requested warehouse does not exist.
pub fn check_consistency(
    session: &mut Session,
    warehouse_ids: &[i64],
) -> Result<Vec<ConsistencyViolation>> {
    if warehouse_ids.iter().any(|warehouse_id| *warehouse_id <= 0) {
        return Err(TransactionError::InvalidInput(
            "warehouse IDs must be positive".into(),
        ));
    }
    if warehouse_ids.is_empty() {
        return Ok(Vec::new());
    }
    for warehouse_id in warehouse_ids {
        if query(
            session,
            &format!("SELECT w_id FROM warehouse WHERE w_id = {warehouse_id}"),
        )?
        .num_rows()
            == 0
        {
            return Err(TransactionError::InvalidInput(format!(
                "warehouse {warehouse_id} was not found"
            )));
        }
    }

    let mut violations = Vec::new();
    let warehouse_where = warehouse_filter(warehouse_ids, "w_id");
    let district_where = warehouse_filter(warehouse_ids, "d_w_id");
    let order_where = warehouse_filter(warehouse_ids, "o_w_id");
    let new_order_where = warehouse_filter(warehouse_ids, "no_w_id");
    let customer_where = warehouse_filter(warehouse_ids, "c_w_id");

    for row in query(
        session,
        &format!(
            "SELECT w.w_id, w.w_ytd, SUM(d.d_ytd) \
             FROM warehouse w LEFT JOIN district d ON d.d_w_id = w.w_id \
             WHERE {warehouse_where} GROUP BY w.w_id, w.w_ytd"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let actual = int(row, 1)?;
        let expected = optional_int(row, 2)?.unwrap_or(0);
        if actual != expected {
            violations.push(violation(
                1,
                warehouse_id,
                None,
                format!("W_YTD is {actual}, but sum(D_YTD) is {expected}"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT d.d_w_id, d.d_id, d.d_next_o_id, MAX(o.o_id), MAX(n.no_o_id) \
             FROM district d JOIN new_order n \
             ON n.no_w_id = d.d_w_id AND n.no_d_id = d.d_id \
             LEFT JOIN orders o ON o.o_w_id = d.d_w_id AND o.o_d_id = d.d_id \
             WHERE {district_where} \
             GROUP BY d.d_w_id, d.d_id, d.d_next_o_id"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let next_order_id = int(row, 2)?;
        let max_order_id = optional_int(row, 3)?;
        let max_new_order_id = optional_int(row, 4)?;
        let expected = next_order_id - 1;
        if max_order_id != Some(expected) || max_new_order_id != Some(expected) {
            violations.push(violation(
                2,
                warehouse_id,
                Some(district_id),
                format!(
                    "D_NEXT_O_ID - 1 is {expected}, max(O_ID) is {max_order_id:?}, \
                     and max(NO_O_ID) is {max_new_order_id:?}"
                ),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT no_w_id, no_d_id, MIN(no_o_id), MAX(no_o_id), COUNT(*) \
             FROM new_order WHERE {new_order_where} GROUP BY no_w_id, no_d_id"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let min_order_id = int(row, 2)?;
        let max_order_id = int(row, 3)?;
        let count = int(row, 4)?;
        let expected = max_order_id - min_order_id + 1;
        if count != expected {
            violations.push(violation(
                3,
                warehouse_id,
                Some(district_id),
                format!("count(NO) is {count}, but max(NO_O_ID) - min(NO_O_ID) + 1 is {expected}"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT o.o_w_id, o.o_d_id, SUM(o.o_ol_cnt), \
             (SELECT COUNT(*) FROM order_line ol \
              WHERE ol.ol_w_id = o.o_w_id AND ol.ol_d_id = o.o_d_id) \
             FROM orders o \
             WHERE {order_where} GROUP BY o.o_w_id, o.o_d_id"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let expected = int(row, 2)?;
        let actual = int(row, 3)?;
        if actual != expected {
            violations.push(violation(
                4,
                warehouse_id,
                Some(district_id),
                format!("count(ORDER_LINE) is {actual}, but sum(O_OL_CNT) is {expected}"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT o.o_w_id, o.o_d_id, o.o_id, o.o_carrier_id, n.no_o_id \
             FROM orders o LEFT JOIN new_order n \
             ON n.no_w_id = o.o_w_id AND n.no_d_id = o.o_d_id AND n.no_o_id = o.o_id \
             WHERE {order_where}"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let order_id = int(row, 2)?;
        let carrier_id = optional_int(row, 3)?;
        let new_order_id = optional_int(row, 4)?;
        if carrier_id.is_none() != new_order_id.is_some() {
            violations.push(violation(
                5,
                warehouse_id,
                Some(district_id),
                format!(
                    "order {order_id} has O_CARRIER_ID {carrier_id:?} and matching \
                     NEW-ORDER row {new_order_id:?}"
                ),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT o.o_w_id, o.o_d_id, o.o_id, o.o_ol_cnt, COUNT(ol.ol_number) \
             FROM orders o LEFT JOIN order_line ol \
             ON ol.ol_w_id = o.o_w_id AND ol.ol_d_id = o.o_d_id AND ol.ol_o_id = o.o_id \
             WHERE {order_where} GROUP BY o.o_w_id, o.o_d_id, o.o_id, o.o_ol_cnt"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let order_id = int(row, 2)?;
        let expected = int(row, 3)?;
        let actual = int(row, 4)?;
        if actual != expected {
            violations.push(violation(
                6,
                warehouse_id,
                Some(district_id),
                format!("order {order_id} has O_OL_CNT {expected}, but {actual} ORDER-LINE rows"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT ol.ol_w_id, ol.ol_d_id, ol.ol_o_id, ol.ol_number, \
             ol.ol_delivery_d, o.o_carrier_id \
             FROM order_line ol JOIN orders o \
             ON o.o_w_id = ol.ol_w_id AND o.o_d_id = ol.ol_d_id AND o.o_id = ol.ol_o_id \
             WHERE {}",
            warehouse_filter(warehouse_ids, "ol.ol_w_id")
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let order_id = int(row, 2)?;
        let line_number = int(row, 3)?;
        let delivery_date = optional_int(row, 4)?;
        let carrier_id = optional_int(row, 5)?;
        if delivery_date.is_none() != carrier_id.is_none() {
            violations.push(violation(
                7,
                warehouse_id,
                Some(district_id),
                format!(
                    "order {order_id} line {line_number} has OL_DELIVERY_D {delivery_date:?} \
                     and O_CARRIER_ID {carrier_id:?}"
                ),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT w.w_id, w.w_ytd, SUM(h.h_amount) \
             FROM warehouse w LEFT JOIN history h ON h.h_w_id = w.w_id \
             WHERE {warehouse_where} GROUP BY w.w_id, w.w_ytd"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let actual = int(row, 1)?;
        let expected = optional_int(row, 2)?.unwrap_or(0);
        if actual != expected {
            violations.push(violation(
                8,
                warehouse_id,
                None,
                format!("W_YTD is {actual}, but sum(H_AMOUNT) is {expected}"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT d.d_w_id, d.d_id, d.d_ytd, SUM(h.h_amount) \
             FROM district d JOIN history h \
             ON h.h_w_id = d.d_w_id AND h.h_d_id = d.d_id \
             WHERE {district_where} GROUP BY d.d_w_id, d.d_id, d.d_ytd"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let actual = int(row, 2)?;
        let expected = optional_int(row, 3)?.unwrap_or(0);
        if actual != expected {
            violations.push(violation(
                9,
                warehouse_id,
                Some(district_id),
                format!("D_YTD is {actual}, but sum(H_AMOUNT) is {expected}"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT c.c_w_id, c.c_d_id, c.c_id, c.c_balance, \
             (SELECT SUM(CASE WHEN ol.ol_delivery_d IS NOT NULL \
                              THEN ol.ol_amount ELSE 0 END) \
              FROM orders o LEFT JOIN order_line ol \
              ON ol.ol_w_id = o.o_w_id AND ol.ol_d_id = o.o_d_id \
              AND ol.ol_o_id = o.o_id \
              WHERE o.o_w_id = c.c_w_id AND o.o_d_id = c.c_d_id \
              AND o.o_c_id = c.c_id), \
             (SELECT SUM(h.h_amount) FROM history h \
              WHERE h.h_c_w_id = c.c_w_id AND h.h_c_d_id = c.c_d_id \
              AND h.h_c_id = c.c_id) \
             FROM customer c WHERE {customer_where}"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let customer_id = int(row, 2)?;
        let actual = int(row, 3)?;
        let delivered_amount = optional_int(row, 4)?.unwrap_or(0);
        let history_amount = optional_int(row, 5)?.unwrap_or(0);
        let expected = delivered_amount - history_amount;
        if actual != expected {
            violations.push(violation(
                10,
                warehouse_id,
                Some(district_id),
                format!(
                    "customer {customer_id} C_BALANCE is {actual}, but delivered OL_AMOUNT \
                     minus H_AMOUNT is {expected}"
                ),
            ));
            break;
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT d.d_w_id, d.d_id, \
             (SELECT COUNT(*) FROM orders o \
              WHERE o.o_w_id = d.d_w_id AND o.o_d_id = d.d_id), \
             (SELECT COUNT(*) FROM new_order n \
              WHERE n.no_w_id = d.d_w_id AND n.no_d_id = d.d_id) \
             FROM district d WHERE {district_where}"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let order_count = int(row, 2)?;
        let new_order_count = int(row, 3)?;
        let actual = order_count - new_order_count;
        if actual != 2100 {
            violations.push(violation(
                11,
                warehouse_id,
                Some(district_id),
                format!("count(ORDER) - count(NEW-ORDER) is {actual}, but expected 2100"),
            ));
        }
    }

    for row in query(
        session,
        &format!(
            "SELECT c.c_w_id, c.c_d_id, c.c_id, c.c_balance, c.c_ytd_payment, \
             SUM(CASE WHEN ol.ol_delivery_d IS NOT NULL THEN ol.ol_amount ELSE 0 END) \
             FROM customer c \
             LEFT JOIN orders o ON o.o_w_id = c.c_w_id AND o.o_d_id = c.c_d_id \
             AND o.o_c_id = c.c_id \
             LEFT JOIN order_line ol ON ol.ol_w_id = o.o_w_id AND ol.ol_d_id = o.o_d_id \
             AND ol.ol_o_id = o.o_id \
             WHERE {customer_where} \
             GROUP BY c.c_w_id, c.c_d_id, c.c_id, c.c_balance, c.c_ytd_payment"
        ),
    )?
    .rows()
    {
        let warehouse_id = int(row, 0)?;
        let district_id = int(row, 1)?;
        let customer_id = int(row, 2)?;
        let balance = int(row, 3)?;
        let ytd_payment = int(row, 4)?;
        let expected = int(row, 5)?;
        let actual = balance + ytd_payment;
        if actual != expected {
            violations.push(violation(
                12,
                warehouse_id,
                Some(district_id),
                format!(
                    "customer {customer_id} C_BALANCE + C_YTD_PAYMENT is {actual}, \
                     but delivered OL_AMOUNT is {expected}"
                ),
            ));
            break;
        }
    }

    Ok(violations)
}
