use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::result::Result;

use htap_common::{HtapError, Result as HtapResult};
use htap_server::Session;

use crate::generate::lineitem;
use crate::generate::orders::{
    assemble_order_shell, eligible_customers, sparse_order_key_in_slice,
};
use crate::generate::part;
use crate::generate::rng::RandomState;
use crate::generate::text::generate_text;
use crate::generate::{assemble_order, Lineitem, Orders};
use crate::load::{format_decimal, format_quantity};
use crate::scale_factor::{scale_factor, ScaleFactorError};

const STREAM_COUNT: u64 = 1_000;
const RF1_STREAM_COUNT: u64 = 3_000;
const RF1_SEED_STRIDE: u64 = 1_000_000;

/// Errors produced while generating TPC-H refresh data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshError {
    StreamOutOfRange,
    ScaleFactorError(ScaleFactorError),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StreamOutOfRange => write!(formatter, "refresh stream is out of range"),
            Self::ScaleFactorError(error) => error.fmt(formatter),
        }
    }
}

impl Error for RefreshError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StreamOutOfRange => None,
            Self::ScaleFactorError(error) => Some(error),
        }
    }
}

impl From<ScaleFactorError> for RefreshError {
    fn from(error: ScaleFactorError) -> Self {
        Self::ScaleFactorError(error)
    }
}

/// The order distribution across one thousand refresh streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRange {
    pub total_orders: u64,
    pub per_stream: u64,
    pub remainder: u64,
}

/// The ORDERS and LINEITEM rows introduced by an RF1 stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rf1Rows {
    pub orders: Vec<Orders>,
    pub lineitems: Vec<Lineitem>,
}

/// Returns the TPC-H refresh stream distribution for a scale factor.
pub fn refresh_range(scale_factor_text: &str) -> Result<RefreshRange, RefreshError> {
    let total_orders = scale_factor(scale_factor_text, 1_500_000)?;
    if total_orders < STREAM_COUNT {
        return Err(RefreshError::StreamOutOfRange);
    }

    Ok(RefreshRange {
        total_orders,
        per_stream: total_orders / STREAM_COUNT,
        remainder: total_orders % STREAM_COUNT,
    })
}

fn rf1_stream_parts(stream: u64, range: RefreshRange) -> Result<(u64, u64, u64), RefreshError> {
    if !(1..=RF1_STREAM_COUNT).contains(&stream) {
        return Err(RefreshError::StreamOutOfRange);
    }

    let stream_index = (stream - 1) % STREAM_COUNT;
    let slice = (stream - 1) / STREAM_COUNT + 1;
    let count = range.per_stream
        + if stream_index == STREAM_COUNT - 1 {
            range.remainder
        } else {
            0
        };

    Ok((stream_index, slice, count))
}

/// Generates the ORDERS rows introduced by an RF1 stream.
///
/// Refresh comments intentionally use normal generated text and do not receive
/// the load-time Query 13 forced comment phrase.
pub fn generate_rf1_rows(
    scale_factor_text: &str,
    stream: u64,
    seed: u64,
) -> Result<Rf1Rows, RefreshError> {
    let range = refresh_range(scale_factor_text)?;
    let (stream_index, slice, count) = rf1_stream_parts(stream, range)?;
    let customer_count = scale_factor(scale_factor_text, 150_000)?;
    let customers = eligible_customers(customer_count);
    let mut rng = RandomState::new(seed.wrapping_add(RF1_SEED_STRIDE.wrapping_mul(stream)));
    let parts = part::generate(&mut rng, scale_factor_text)?;

    let mut shells = Vec::with_capacity(count as usize);
    for offset in 0..count {
        let order_index = stream_index * range.per_stream + offset;
        let custkey = customers[rng.structural_bounded(customers.len() as u64) as usize];
        let comment = generate_text(&mut rng, 19, 78);
        shells.push(assemble_order_shell(
            &mut rng,
            scale_factor_text,
            sparse_order_key_in_slice(order_index, slice),
            custkey,
            comment,
        )?);
    }

    let lineitems = lineitem::generate(&mut rng, scale_factor_text, &shells, &parts, &[])?;
    let mut lineitems_by_order = BTreeMap::<i64, Vec<&Lineitem>>::new();
    for lineitem in &lineitems {
        lineitems_by_order
            .entry(lineitem.l_orderkey)
            .or_default()
            .push(lineitem);
    }

    let orders = shells
        .iter()
        .map(|shell| {
            let (o_totalprice, o_orderstatus) =
                assemble_order(&lineitems_by_order[&shell.o_orderkey]);
            Orders {
                o_orderkey: shell.o_orderkey,
                o_custkey: shell.o_custkey,
                o_orderstatus,
                o_totalprice,
                o_orderdate: shell.o_orderdate.clone(),
                o_orderpriority: shell.o_orderpriority.clone(),
                o_clerk: shell.o_clerk.clone(),
                o_shippriority: shell.o_shippriority,
                o_comment: shell.o_comment.clone(),
            }
        })
        .collect();

    Ok(Rf1Rows { orders, lineitems })
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Inserts one order and all of its lineitems in a single transaction.
pub fn apply_one_order_insert(
    session: &mut Session,
    order: &Orders,
    lineitems: &[Lineitem],
) -> HtapResult<()> {
    session.begin()?;

    let result = (|| -> HtapResult<()> {
        let order_sql = format!(
            "INSERT INTO orders \
             (o_orderkey, o_custkey, o_orderstatus, o_totalprice, \
              o_orderdate, o_orderpriority, o_clerk, o_shippriority, o_comment) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {})",
            order.o_orderkey,
            order.o_custkey,
            sql_literal(&order.o_orderstatus),
            format_decimal(order.o_totalprice),
            sql_literal(&order.o_orderdate),
            sql_literal(&order.o_orderpriority),
            sql_literal(&order.o_clerk),
            order.o_shippriority,
            sql_literal(&order.o_comment),
        );
        session.execute(&order_sql)?;

        if !lineitems.is_empty() {
            let values = lineitems
                .iter()
                .map(|lineitem| {
                    format!(
                        "({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
                        lineitem.l_orderkey,
                        lineitem.l_linenumber,
                        lineitem.l_partkey,
                        lineitem.l_suppkey,
                        format_quantity(lineitem.l_quantity),
                        format_decimal(lineitem.l_extendedprice),
                        format_decimal(lineitem.l_discount),
                        format_decimal(lineitem.l_tax),
                        sql_literal(&lineitem.l_returnflag),
                        sql_literal(&lineitem.l_linestatus),
                        sql_literal(&lineitem.l_shipdate),
                        sql_literal(&lineitem.l_commitdate),
                        sql_literal(&lineitem.l_receiptdate),
                        sql_literal(&lineitem.l_shipinstruct),
                        sql_literal(&lineitem.l_shipmode),
                        sql_literal(&lineitem.l_comment),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            let lineitem_sql = format!(
                "INSERT INTO lineitem \
                 (l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity, \
                  l_extendedprice, l_discount, l_tax, l_returnflag, l_linestatus, \
                  l_shipdate, l_commitdate, l_receiptdate, l_shipinstruct, \
                  l_shipmode, l_comment) \
                 VALUES {values}"
            );
            session.execute(&lineitem_sql)?;
        }

        session.commit()?;
        Ok(())
    })();

    if let Err(error) = result {
        session.rollback()?;
        return Err(error);
    }

    Ok(())
}

/// Deletes one order and all of its lineitems in a single transaction.
pub fn apply_one_order_delete(session: &mut Session, order_key: i64) -> HtapResult<()> {
    session.begin()?;

    let result = (|| -> HtapResult<()> {
        for line_number in 1..=7 {
            let lineitem_sql = format!(
                "DELETE FROM lineitem \
                 WHERE l_orderkey = {order_key} AND l_linenumber = {line_number}"
            );
            session.execute(&lineitem_sql)?;
        }

        let order_sql = format!("DELETE FROM orders WHERE o_orderkey = {order_key}");
        session.execute(&order_sql)?;

        session.commit()?;
        Ok(())
    })();

    if let Err(error) = result {
        session.rollback()?;
        return Err(error);
    }

    Ok(())
}

/// Applies all new-order inserts for one RF1 stream.
pub fn rf1_new_sales(
    session: &mut Session,
    scale_factor_text: &str,
    stream: u64,
    seed: u64,
) -> HtapResult<()> {
    let rows = generate_rf1_rows(scale_factor_text, stream, seed)
        .map_err(|error| HtapError::InvalidArgument(error.to_string()))?;

    let mut lineitems_by_order = BTreeMap::<i64, Vec<Lineitem>>::new();
    for lineitem in rows.lineitems {
        lineitems_by_order
            .entry(lineitem.l_orderkey)
            .or_default()
            .push(lineitem);
    }

    for order in &rows.orders {
        let lineitems = lineitems_by_order
            .get(&order.o_orderkey)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        apply_one_order_insert(session, order, lineitems)?;
    }

    Ok(())
}

/// Applies all old-order deletes for one RF2 stream.
pub fn rf2_old_sales(
    session: &mut Session,
    scale_factor_text: &str,
    stream: u64,
) -> HtapResult<()> {
    let order_keys = generate_rf2_plan(scale_factor_text, stream)
        .map_err(|error| HtapError::InvalidArgument(error.to_string()))?;

    for order_key in order_keys {
        apply_one_order_delete(session, order_key)?;
    }

    Ok(())
}

/// Returns the existing load-order keys selected for deletion by an RF2 stream.
pub fn generate_rf2_plan(scale_factor_text: &str, stream: u64) -> Result<Vec<i64>, RefreshError> {
    let range = refresh_range(scale_factor_text)?;
    if !(1..=STREAM_COUNT).contains(&stream) {
        return Err(RefreshError::StreamOutOfRange);
    }

    let stream_index = stream - 1;
    let count = range.per_stream
        + if stream_index == STREAM_COUNT - 1 {
            range.remainder
        } else {
            0
        };
    let first_index = stream_index * range.per_stream;

    Ok((0..count)
        .map(|offset| sparse_order_key_in_slice(first_index + offset, 0) as i64)
        .collect())
}

#[cfg(test)]
mod tests {
    use crate::generate::orders;
    use crate::generate::rng::RandomState;

    use super::{generate_rf1_rows, generate_rf2_plan, refresh_range, RefreshError};

    #[test]
    fn rf1_counts_and_generation_are_deterministic() {
        let rows = generate_rf1_rows("0.01", 1, 42).unwrap();

        assert_eq!(
            rows.orders.len(),
            refresh_range("0.01").unwrap().per_stream as usize
        );
        assert_eq!(rows, generate_rf1_rows("0.01", 1, 42).unwrap());
    }

    #[test]
    fn rf1_generation_with_maximum_seed_is_deterministic() {
        let rows = generate_rf1_rows("0.01", 1, u64::MAX).unwrap();

        assert_eq!(rows, generate_rf1_rows("0.01", 1, u64::MAX).unwrap());
    }

    #[test]
    fn rf1_streams_use_the_expected_sparse_slices() {
        let stream_one = generate_rf1_rows("0.01", 1, 42).unwrap();
        assert!(stream_one
            .orders
            .iter()
            .all(|row| (9..=16).contains(&(row.o_orderkey % 32))));

        let stream_one_thousand_one = generate_rf1_rows("0.01", 1_001, 42).unwrap();
        assert!(stream_one_thousand_one
            .orders
            .iter()
            .all(|row| (17..=24).contains(&(row.o_orderkey % 32))));
    }

    #[test]
    fn out_of_range_streams_are_rejected() {
        assert_eq!(
            generate_rf1_rows("0.01", 0, 42),
            Err(RefreshError::StreamOutOfRange)
        );
        assert_eq!(
            generate_rf1_rows("0.01", 3_001, 42),
            Err(RefreshError::StreamOutOfRange)
        );
        assert_eq!(
            generate_rf2_plan("0.01", 0),
            Err(RefreshError::StreamOutOfRange)
        );
        assert_eq!(
            generate_rf2_plan("0.01", 1_001),
            Err(RefreshError::StreamOutOfRange)
        );
    }

    #[test]
    fn rf2_stream_one_keys_match_the_load_order_key_range() {
        let mut rng = RandomState::new(42);
        let load_shells = orders::generate(&mut rng, "0.01").unwrap();
        let rf2_keys = generate_rf2_plan("0.01", 1).unwrap();
        let expected: Vec<_> = load_shells
            .iter()
            .take(rf2_keys.len())
            .map(|shell| shell.o_orderkey)
            .collect();

        assert_eq!(rf2_keys, expected);
    }
}
