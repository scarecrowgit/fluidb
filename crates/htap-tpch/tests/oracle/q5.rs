use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q5_END, Q5_START};
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
            order.o_orderdate.as_str() >= Q5_START && order.o_orderdate.as_str() < Q5_END
        })
        .map(|order| (order.o_orderkey, order))
        .collect();
    let suppliers: BTreeMap<_, _> = dataset
        .supplier
        .iter()
        .map(|supplier| (supplier.s_suppkey, supplier))
        .collect();
    let nations: BTreeMap<_, _> = dataset
        .nation
        .iter()
        .map(|nation| (nation.n_nationkey, nation))
        .collect();
    let asia_nation_keys: Vec<_> = dataset
        .nation
        .iter()
        .filter(|nation| {
            dataset
                .region
                .iter()
                .any(|region| region.r_regionkey == nation.n_regionkey && region.r_name == "ASIA")
        })
        .map(|nation| nation.n_nationkey)
        .collect();

    let mut revenues = BTreeMap::<String, Dec>::new();
    for line in dataset.lineitem.iter() {
        let Some(order) = orders.get(&line.l_orderkey) else {
            continue;
        };
        let customer = customers[&order.o_custkey];
        let supplier = suppliers[&line.l_suppkey];
        if customer.c_nationkey != supplier.s_nationkey
            || !asia_nation_keys.contains(&supplier.s_nationkey)
        {
            continue;
        }

        let nation = nations[&supplier.s_nationkey];
        let price = Dec::new(i128::from(line.l_extendedprice), 15, 2);
        let discount = Dec::new(i128::from(line.l_discount), 15, 2);
        let revenue = price.mul(Dec::integer(1).sub(discount));
        revenues
            .entry(nation.n_name.clone())
            .and_modify(|total| *total = total.add(revenue).with_precision(18))
            .or_insert(revenue.with_precision(18));
    }

    let mut rows: Vec<_> = revenues
        .into_iter()
        .map(|(nation, revenue)| vec![Value::String(nation), value(revenue)])
        .collect();
    rows.sort_by(|left, right| {
        value_decimal(&right[1])
            .partial_cmp(&value_decimal(&left[1]))
            .unwrap()
    });
    rows
}

super::verify_non_empty!();

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
