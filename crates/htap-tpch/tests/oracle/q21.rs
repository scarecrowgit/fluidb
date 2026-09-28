use std::collections::{BTreeMap, BTreeSet};

use htap_common::types::Value;

/// Q21 result types: s_name String, numwait Int64.
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .filter(|order| order.o_orderstatus == "F")
        .map(|order| (order.o_orderkey, order))
        .collect();
    let nations: BTreeMap<_, _> = dataset
        .nation
        .iter()
        .map(|nation| (nation.n_nationkey, nation))
        .collect();
    let saudi_suppliers: BTreeSet<_> = dataset
        .supplier
        .iter()
        .filter(|supplier| nations[&supplier.s_nationkey].n_name == "SAUDI ARABIA")
        .map(|supplier| supplier.s_suppkey)
        .collect();
    let suppliers: BTreeMap<_, _> = dataset
        .supplier
        .iter()
        .map(|supplier| (supplier.s_suppkey, supplier))
        .collect();

    let by_order =
        dataset
            .lineitem
            .iter()
            .fold(BTreeMap::<i64, Vec<_>>::new(), |mut lines, line| {
                lines.entry(line.l_orderkey).or_default().push(line);
                lines
            });
    let mut counts = BTreeMap::<i64, i64>::new();

    for (orderkey, lines) in by_order {
        if !orders.contains_key(&orderkey) {
            continue;
        }
        for line in &lines {
            if !saudi_suppliers.contains(&line.l_suppkey) || line.l_receiptdate <= line.l_commitdate
            {
                continue;
            }

            let other_suppliers = lines
                .iter()
                .filter(|other| other.l_suppkey != line.l_suppkey)
                .collect::<Vec<_>>();
            if other_suppliers.is_empty()
                || other_suppliers
                    .iter()
                    .any(|other| other.l_receiptdate > other.l_commitdate)
            {
                continue;
            }
            *counts.entry(line.l_suppkey).or_default() += 1;
        }
    }

    let mut rows: Vec<_> = counts
        .into_iter()
        .map(|(supplier_key, count)| {
            vec![
                Value::String(suppliers[&supplier_key].s_name.clone()),
                Value::Int64(count),
            ]
        })
        .collect();
    rows.sort_by(|left, right| {
        integer(&right[1])
            .cmp(&integer(&left[1]))
            .then_with(|| string(&left[0]).cmp(string(&right[0])))
    });
    rows
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    let rows = expected(dataset);

    if rows.is_empty() {
        return Err("Q21 result must contain at least one supplier".to_string());
    }
    if !rows.iter().any(|row| {
        matches!(
            row.get(1),
            Some(Value::Int64(numwait)) if *numwait >= 1
        )
    }) {
        return Err("Q21 result must contain a supplier with numwait >= 1".to_string());
    }

    Ok(())
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
