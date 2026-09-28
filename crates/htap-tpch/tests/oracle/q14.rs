use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q14_END, Q14_START};
use super::decimal::Dec;

/// Q14 result type: promo_revenue DECIMAL(18,10).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let parts: BTreeMap<_, _> = dataset
        .part
        .iter()
        .map(|part| (part.p_partkey, part))
        .collect();
    let mut promotional = Dec::new(0, 18, 4);
    let mut total = Dec::new(0, 18, 4);

    for line in dataset
        .lineitem
        .iter()
        .filter(|line| line.l_shipdate.as_str() >= Q14_START && line.l_shipdate.as_str() < Q14_END)
    {
        let revenue = Dec::new(i128::from(line.l_extendedprice), 15, 2)
            .mul(Dec::integer(1).sub(Dec::new(i128::from(line.l_discount), 15, 2)))
            .with_precision(18);
        total = total.add(revenue).with_precision(18);
        if parts[&line.l_partkey].p_type.starts_with("PROMO") {
            promotional = promotional.add(revenue).with_precision(18);
        }
    }

    // The final DECIMAL(18,10) percentage division is where Q14 rounds.
    let percentage = promotional
        .mul(Dec::new(10000, 4, 2))
        .div(total)
        .with_precision(18);
    vec![vec![Value::Decimal {
        value: percentage.value as i64,
        precision: percentage.precision,
        scale: percentage.scale,
    }]]
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    let result = expected(dataset);
    if result.is_empty() {
        return Err("Q14 coverage requires a non-empty expected result".to_string());
    }
    if result[0].is_empty() || matches!(result[0][0], Value::Null) {
        return Err("Q14 coverage requires a non-NULL aggregate value".to_string());
    }
    Ok(())
}
