use std::collections::BTreeMap;

use htap_common::types::Value;

use super::decimal::Dec;
use super::lineitem_quantity;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let matching_parts: Vec<_> = dataset
        .part
        .iter()
        .filter(|part| part.p_brand == "Brand#23" && part.p_container == "MED BOX")
        .map(|part| part.p_partkey)
        .collect();

    let mut quantities = BTreeMap::<i64, Vec<Dec>>::new();
    for line in dataset
        .lineitem
        .iter()
        .filter(|line| matching_parts.contains(&line.l_partkey))
    {
        quantities
            .entry(line.l_partkey)
            .or_default()
            .push(lineitem_quantity(line.l_quantity));
    }

    let averages: BTreeMap<_, _> = quantities
        .iter()
        .map(|(partkey, values)| (*partkey, Dec::avg(values.iter().copied()).unwrap()))
        .collect();

    let qualifying_prices = dataset.lineitem.iter().filter_map(|line| {
        let average = averages.get(&line.l_partkey)?;
        // The correlated AVG is rounded before multiplication by the 0.2 literal.
        let threshold = Dec::new(2, 1, 1).mul(*average);
        let quantity = Dec::new(i128::from(line.l_quantity) * 100, 15, 2);
        (quantity < threshold).then_some(Dec::new(i128::from(line.l_extendedprice), 15, 2))
    });

    let result = Dec::sum(qualifying_prices).map(|sum| {
        // The final division by 7.0 rounds once to DECIMAL(18,6).
        sum.div(Dec::new(70, 2, 1))
    });

    vec![vec![match result {
        Some(decimal) => Value::Decimal {
            value: decimal.value as i64,
            precision: decimal.precision,
            scale: decimal.scale,
        },
        None => Value::Null,
    }]]
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    let result = expected(dataset);
    if result.is_empty() {
        return Err("Q17 coverage requires a non-empty expected result".to_string());
    }
    if result[0].is_empty() || matches!(result[0][0], Value::Null) {
        return Err("Q17 coverage requires a non-NULL aggregate value".to_string());
    }
    Ok(())
}
