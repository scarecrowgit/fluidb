use std::collections::BTreeMap;

use htap_common::types::Value;

use super::decimal::Dec;

/// Q9 result types: nation String, o_year Int64, sum_profit DECIMAL(18,4).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let parts: BTreeMap<_, _> = dataset
        .part
        .iter()
        .filter(|part| part.p_name.contains("green"))
        .map(|part| (part.p_partkey, part))
        .collect();
    let supplies: BTreeMap<_, _> = dataset
        .partsupp
        .iter()
        .map(|supply| ((supply.ps_partkey, supply.ps_suppkey), supply))
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
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .map(|order| (order.o_orderkey, order))
        .collect();

    let mut profits = BTreeMap::<(String, i64), Dec>::new();
    for line in &dataset.lineitem {
        if !parts.contains_key(&line.l_partkey) {
            continue;
        }
        let Some(supply) = supplies.get(&(line.l_partkey, line.l_suppkey)) else {
            continue;
        };
        let supplier = suppliers[&line.l_suppkey];
        let nation = nations[&supplier.s_nationkey];
        let order = orders[&line.l_orderkey];
        let year = order.o_orderdate[..4]
            .parse::<i64>()
            .expect("valid order year");
        let revenue = Dec::new(i128::from(line.l_extendedprice), 15, 2)
            .mul(Dec::integer(1).sub(Dec::new(i128::from(line.l_discount), 15, 2)));
        let cost = Dec::new(i128::from(supply.ps_supplycost), 15, 2)
            .mul(super::lineitem_quantity(line.l_quantity));
        let profit = revenue.sub(cost).with_precision(18);

        profits
            .entry((nation.n_name.clone(), year))
            .and_modify(|total| *total = total.add(profit).with_precision(18))
            .or_insert(profit);
    }

    let mut rows: Vec<_> = profits
        .into_iter()
        .map(|((nation, year), profit)| {
            vec![Value::String(nation), Value::Int64(year), value(profit)]
        })
        .collect();
    rows.sort_by(|left, right| {
        string(&left[0])
            .cmp(string(&right[0]))
            .then_with(|| integer(&right[1]).cmp(&integer(&left[1])))
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

fn string(value: &Value) -> &str {
    match value {
        Value::String(value) => value,
        _ => unreachable!(),
    }
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        _ => unreachable!(),
    }
}

super::verify_non_empty!();
