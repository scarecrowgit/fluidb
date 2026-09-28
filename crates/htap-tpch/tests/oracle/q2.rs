use std::collections::BTreeMap;

use htap_common::types::Value;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let nations: BTreeMap<_, _> = dataset
        .nation
        .iter()
        .map(|nation| (nation.n_nationkey, nation))
        .collect();
    let suppliers: BTreeMap<_, _> = dataset
        .supplier
        .iter()
        .map(|supplier| (supplier.s_suppkey, supplier))
        .collect();

    let mut candidates = Vec::new();
    for part in dataset
        .part
        .iter()
        .filter(|part| part.p_size == 15 && part.p_type.ends_with("BRASS"))
    {
        for supply in dataset
            .partsupp
            .iter()
            .filter(|supply| supply.ps_partkey == part.p_partkey)
        {
            let supplier = suppliers[&supply.ps_suppkey];
            let nation = nations[&supplier.s_nationkey];
            if nation.n_regionkey == 3 {
                candidates.push((part, supply, supplier, nation));
            }
        }
    }

    let mut minimum_costs = BTreeMap::<i64, i64>::new();
    for (part, supply, _, _) in &candidates {
        minimum_costs
            .entry(part.p_partkey)
            .and_modify(|cost| *cost = (*cost).min(supply.ps_supplycost))
            .or_insert(supply.ps_supplycost);
    }

    let mut rows: Vec<_> = candidates
        .into_iter()
        .filter(|(part, supply, _, _)| minimum_costs[&part.p_partkey] == supply.ps_supplycost)
        .map(|(part, _, supplier, nation)| {
            vec![
                Value::Decimal {
                    value: supplier.s_acctbal,
                    precision: 15,
                    scale: 2,
                },
                Value::String(supplier.s_name.clone()),
                Value::String(nation.n_name.clone()),
                Value::Int64(part.p_partkey),
                Value::String(part.p_mfgr.clone()),
                Value::String(supplier.s_address.clone()),
                Value::String(supplier.s_phone.clone()),
                Value::String(supplier.s_comment.clone()),
            ]
        })
        .collect();

    rows.sort_by(|left, right| {
        let left_balance = match &left[0] {
            Value::Decimal { value, .. } => *value,
            _ => unreachable!(),
        };
        let right_balance = match &right[0] {
            Value::Decimal { value, .. } => *value,
            _ => unreachable!(),
        };
        right_balance
            .cmp(&left_balance)
            .then_with(|| value_string(&left[2]).cmp(value_string(&right[2])))
            .then_with(|| value_string(&left[1]).cmp(value_string(&right[1])))
            .then_with(|| value_i64(&left[3]).cmp(&value_i64(&right[3])))
    });
    rows.truncate(100);
    rows
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    if expected(dataset).is_empty() {
        Err("Q2 has no expected results for this dataset".to_string())
    } else {
        Ok(())
    }
}

fn value_string(value: &Value) -> &str {
    match value {
        Value::String(value) => value,
        _ => unreachable!(),
    }
}

fn value_i64(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        _ => unreachable!(),
    }
}
