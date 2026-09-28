use std::collections::BTreeMap;

use htap_common::types::{parse_date_to_timestamp_micros, Value};

use super::decimal::Dec;
use super::lineitem_quantity;

/// Q18 result types: customer fields, order fields, and SUM(l_quantity) DECIMAL(18,2).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_quantity: DECIMAL(15,2), represented by lineitem_quantity.
    // SUM(l_quantity): DECIMAL(18,2); no rounding occurs.
    let quantities: BTreeMap<_, _> = dataset
        .lineitem
        .iter()
        .fold(BTreeMap::<i64, Vec<Dec>>::new(), |mut totals, line| {
            totals
                .entry(line.l_orderkey)
                .or_default()
                .push(lineitem_quantity(line.l_quantity));
            totals
        })
        .into_iter()
        .filter_map(|(orderkey, quantities)| {
            let total = Dec::sum(quantities).expect("order has lineitems");
            (total > Dec::integer(300)).then_some((orderkey, total))
        })
        .collect();

    let customers: BTreeMap<_, _> = dataset
        .customer
        .iter()
        .map(|customer| (customer.c_custkey, customer))
        .collect();
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .map(|order| (order.o_orderkey, order))
        .collect();

    let mut rows: Vec<_> = quantities
        .into_iter()
        .map(|(orderkey, quantity)| {
            let order = orders[&orderkey];
            let customer = customers[&order.o_custkey];
            vec![
                Value::String(customer.c_name.clone()),
                Value::Int64(customer.c_custkey),
                Value::Int64(order.o_orderkey),
                Value::Timestamp(
                    parse_date_to_timestamp_micros(&order.o_orderdate).expect("valid date"),
                ),
                value(Dec::new(i128::from(order.o_totalprice), 15, 2)),
                value(quantity),
            ]
        })
        .collect();
    rows.sort_by(|left, right| {
        decimal(&right[4])
            .partial_cmp(&decimal(&left[4]))
            .unwrap()
            .then_with(|| timestamp(&left[3]).cmp(&timestamp(&right[3])))
    });
    rows
}

fn value(decimal: Dec) -> Value {
    Value::Decimal {
        value: decimal.value as i64,
        precision: decimal.precision,
        scale: decimal.scale,
    }
}

fn decimal(value: &Value) -> Dec {
    match value {
        Value::Decimal {
            value,
            precision,
            scale,
        } => Dec::new(i128::from(*value), *precision, *scale),
        _ => unreachable!(),
    }
}

fn timestamp(value: &Value) -> i64 {
    match value {
        Value::Timestamp(value) => *value,
        _ => unreachable!(),
    }
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    if expected(dataset).is_empty() {
        Err("expected result must be non-empty for coverage".to_string())
    } else {
        Ok(())
    }
}
