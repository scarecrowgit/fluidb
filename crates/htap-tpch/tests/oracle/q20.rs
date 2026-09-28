use std::collections::{BTreeMap, BTreeSet};

use htap_common::types::Value;

use super::dates::{Q6_END, Q6_START};
use super::decimal::Dec;
use super::lineitem_quantity;

/// Q20 result types: s_name String, s_address String.
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_quantity: DECIMAL(15,2).
    // SUM(l_quantity): DECIMAL(18,2).
    // 0.5: DECIMAL(1,1).
    // 0.5 * SUM(l_quantity): DECIMAL(18,3); no rounding occurs.
    let forest_parts: BTreeSet<_> = dataset
        .part
        .iter()
        .filter(|part| part.p_name.starts_with("forest"))
        .map(|part| part.p_partkey)
        .collect();
    let shipped_quantities = dataset
        .lineitem
        .iter()
        .filter(|line| line.l_shipdate.as_str() >= Q6_START && line.l_shipdate.as_str() < Q6_END)
        .fold(BTreeMap::<(i64, i64), Vec<Dec>>::new(), |mut sums, line| {
            sums.entry((line.l_partkey, line.l_suppkey))
                .or_default()
                .push(lineitem_quantity(line.l_quantity));
            sums
        });
    let nations: BTreeMap<_, _> = dataset
        .nation
        .iter()
        .map(|nation| (nation.n_nationkey, nation))
        .collect();

    let qualifying_suppliers: BTreeSet<_> = dataset
        .partsupp
        .iter()
        .filter(|supply| forest_parts.contains(&supply.ps_partkey))
        .filter_map(|supply| {
            // A missing correlated SUM is NULL, making availqty > NULL UNKNOWN.
            let sum = Dec::sum(
                shipped_quantities
                    .get(&(supply.ps_partkey, supply.ps_suppkey))?
                    .iter()
                    .copied(),
            )?;
            let threshold = Dec::new(5, 1, 1).mul(sum);
            (Dec::integer(i128::from(supply.ps_availqty)) > threshold).then_some(supply.ps_suppkey)
        })
        .collect();

    let mut rows: Vec<_> = dataset
        .supplier
        .iter()
        .filter(|supplier| qualifying_suppliers.contains(&supplier.s_suppkey))
        .filter(|supplier| nations[&supplier.s_nationkey].n_name == "CANADA")
        .map(|supplier| {
            vec![
                Value::String(supplier.s_name.clone()),
                Value::String(supplier.s_address.clone()),
            ]
        })
        .collect();
    rows.sort_by(|left, right| string(&left[0]).cmp(string(&right[0])));
    rows
}

/// Ensures the NULL path of Q20's correlated SUM is represented.
pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    let forest_parts: BTreeSet<_> = dataset
        .part
        .iter()
        .filter(|part| part.p_name.starts_with("forest"))
        .map(|part| part.p_partkey)
        .collect();
    let canadian_suppliers: BTreeSet<_> = dataset
        .supplier
        .iter()
        .filter(|supplier| {
            dataset.nation.iter().any(|nation| {
                nation.n_nationkey == supplier.s_nationkey && nation.n_name == "CANADA"
            })
        })
        .map(|supplier| supplier.s_suppkey)
        .collect();
    let shipped_pairs: BTreeSet<_> = dataset
        .lineitem
        .iter()
        .filter(|line| line.l_shipdate.as_str() >= Q6_START && line.l_shipdate.as_str() < Q6_END)
        .map(|line| (line.l_partkey, line.l_suppkey))
        .collect();

    dataset
        .partsupp
        .iter()
        .any(|supply| {
            forest_parts.contains(&supply.ps_partkey)
                && canadian_suppliers.contains(&supply.ps_suppkey)
                && !shipped_pairs.contains(&(supply.ps_partkey, supply.ps_suppkey))
        })
        .then_some(())
        .ok_or_else(|| {
            "Q20 coverage requires a Canadian forest partsupp pair without Q6-window lineitems"
                .to_string()
        })
}

fn string(value: &Value) -> &str {
    match value {
        Value::String(value) => value,
        _ => unreachable!(),
    }
}
