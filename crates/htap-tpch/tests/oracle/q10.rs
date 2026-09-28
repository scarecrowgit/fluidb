use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q10_END, Q10_START};
use super::decimal::Dec;

/// Q10 result types: customer fields plus revenue DECIMAL(18,4).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let customers: BTreeMap<_, _> = dataset
        .customer
        .iter()
        .map(|customer| (customer.c_custkey, customer))
        .collect();
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .filter(|order| {
            order.o_orderdate.as_str() >= Q10_START && order.o_orderdate.as_str() < Q10_END
        })
        .map(|order| (order.o_orderkey, order))
        .collect();
    let nations: BTreeMap<_, _> = dataset
        .nation
        .iter()
        .map(|nation| (nation.n_nationkey, nation))
        .collect();

    let mut revenues = BTreeMap::<i64, Dec>::new();
    for line in dataset
        .lineitem
        .iter()
        .filter(|line| line.l_returnflag == "R")
    {
        let Some(order) = orders.get(&line.l_orderkey) else {
            continue;
        };
        let revenue = Dec::new(i128::from(line.l_extendedprice), 15, 2)
            .mul(Dec::integer(1).sub(Dec::new(i128::from(line.l_discount), 15, 2)))
            .with_precision(18);
        revenues
            .entry(order.o_custkey)
            .and_modify(|total| *total = total.add(revenue).with_precision(18))
            .or_insert(revenue);
    }

    let mut rows: Vec<_> = revenues
        .into_iter()
        .map(|(customer_key, revenue)| {
            let customer = customers[&customer_key];
            let nation = nations[&customer.c_nationkey];
            vec![
                Value::Int64(customer.c_custkey),
                Value::String(customer.c_name.clone()),
                value(revenue),
                value(Dec::new(i128::from(customer.c_acctbal), 15, 2)),
                Value::String(nation.n_name.clone()),
                Value::String(customer.c_address.clone()),
                Value::String(customer.c_phone.clone()),
                Value::String(customer.c_comment.clone()),
            ]
        })
        .collect();
    rows.sort_by(|left, right| decimal(&right[2]).partial_cmp(&decimal(&left[2])).unwrap());
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

super::verify_non_empty!();
