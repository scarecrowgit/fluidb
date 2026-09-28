use std::collections::BTreeMap;

use htap_common::types::Value;

use super::decimal::Dec;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let german_nation_keys: Vec<_> = dataset
        .nation
        .iter()
        .filter(|nation| nation.n_name == "GERMANY")
        .map(|nation| nation.n_nationkey)
        .collect();

    let german_supplier_keys: Vec<_> = dataset
        .supplier
        .iter()
        .filter(|supplier| german_nation_keys.contains(&supplier.s_nationkey))
        .map(|supplier| supplier.s_suppkey)
        .collect();

    let mut values = BTreeMap::<i64, Dec>::new();
    for supply in dataset
        .partsupp
        .iter()
        .filter(|supply| german_supplier_keys.contains(&supply.ps_suppkey))
    {
        let supply_cost = Dec::new(i128::from(supply.ps_supplycost), 15, 2);
        let quantity = Dec::integer(i128::from(supply.ps_availqty));
        let value = supply_cost.mul(quantity).with_precision(18);
        values
            .entry(supply.ps_partkey)
            .and_modify(|total| *total = total.add(value).with_precision(18))
            .or_insert(value);
    }

    let total = Dec::sum(values.values().copied()).unwrap();
    // Query 11 at SF 0.01 evaluates 0.0001 / 0.01 to 0.01.
    let threshold = total.mul(Dec::new(1, 2, 2)).with_precision(18);

    let mut rows: Vec<_> = values
        .into_iter()
        .filter(|(_, value)| *value > threshold)
        .map(|(partkey, value)| {
            vec![
                Value::Int64(partkey),
                Value::Decimal {
                    value: value.value as i64,
                    precision: 18,
                    scale: value.scale,
                },
            ]
        })
        .collect();

    rows.sort_by(|left, right| {
        let left_value = match &left[1] {
            Value::Decimal { value, .. } => *value,
            _ => unreachable!(),
        };
        let right_value = match &right[1] {
            Value::Decimal { value, .. } => *value,
            _ => unreachable!(),
        };
        right_value.cmp(&left_value)
    });
    rows
}

super::verify_non_empty!();
