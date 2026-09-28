use std::collections::BTreeMap;

use htap_common::types::{parse_date_to_timestamp_micros, Value};

use super::dates::{Q3_ORDERDATE_CUTOFF, Q3_SHIPDATE_CUTOFF};
use super::decimal::Dec;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_extendedprice: DECIMAL(15,2); l_discount: DECIMAL(15,2).
    // 1 - l_discount: DECIMAL(15,2).
    // l_extendedprice * (1 - l_discount): DECIMAL(18,4).
    // SUM(revenue): DECIMAL(18,4); no rounding occurs.
    let customers: BTreeMap<_, _> = dataset
        .customer
        .iter()
        .map(|customer| (customer.c_custkey, customer))
        .collect();
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .filter(|order| {
            customers
                .get(&order.o_custkey)
                .is_some_and(|customer| customer.c_mktsegment == "BUILDING")
                && order.o_orderdate.as_str() < Q3_ORDERDATE_CUTOFF
        })
        .map(|order| (order.o_orderkey, order))
        .collect();

    let mut revenues = BTreeMap::<(i64, String, i32), Dec>::new();
    for line in dataset.lineitem.iter().filter(|line| {
        line.l_shipdate.as_str() > Q3_SHIPDATE_CUTOFF && orders.contains_key(&line.l_orderkey)
    }) {
        let order = orders[&line.l_orderkey];
        let price = Dec::new(i128::from(line.l_extendedprice), 15, 2);
        let discount = Dec::new(i128::from(line.l_discount), 15, 2);
        let revenue = price.mul(Dec::integer(1).sub(discount));
        revenues
            .entry((
                line.l_orderkey,
                order.o_orderdate.clone(),
                order.o_shippriority,
            ))
            .and_modify(|total| *total = total.add(revenue).with_precision(18))
            .or_insert(revenue.with_precision(18));
    }

    let mut rows: Vec<_> = revenues
        .into_iter()
        .map(|((orderkey, orderdate, shippriority), revenue)| {
            vec![
                Value::Int64(orderkey),
                value(revenue),
                Value::Timestamp(parse_date_to_timestamp_micros(&orderdate).expect("valid date")),
                Value::Int32(shippriority),
            ]
        })
        .collect();
    rows.sort_by(|left, right| {
        value_decimal(&right[1])
            .partial_cmp(&value_decimal(&left[1]))
            .unwrap()
            .then_with(|| value_string(&left[2]).cmp(value_string(&right[2])))
    });
    rows
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    if expected(dataset).is_empty() {
        Err("Q3 has no expected results for this dataset".to_string())
    } else {
        Ok(())
    }
}

fn value(decimal: Dec) -> Value {
    Value::Decimal {
        value: decimal.value as i64,
        precision: decimal.precision,
        scale: decimal.scale,
    }
}

fn value_decimal(value: &Value) -> Dec {
    match value {
        Value::Decimal {
            value,
            precision,
            scale,
        } => Dec::new(i128::from(*value), *precision, *scale),
        _ => unreachable!(),
    }
}

fn value_string(value: &Value) -> &str {
    match value {
        Value::String(value) => value,
        _ => unreachable!(),
    }
}
