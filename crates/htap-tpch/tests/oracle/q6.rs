use htap_common::types::Value;

use super::dates::{Q6_END, Q6_START};
use super::decimal::Dec;
use super::lineitem_quantity;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_extendedprice and l_discount are DECIMAL(15,2).
    // l_extendedprice * l_discount is DECIMAL(18,4).
    // SUM(revenue) is DECIMAL(18,4); no rounding occurs.
    let revenue = Dec::sum(dataset.lineitem.iter().filter_map(|line| {
        let discount = Dec::new(i128::from(line.l_discount), 15, 2);
        (line.l_shipdate.as_str() >= Q6_START
            && line.l_shipdate.as_str() < Q6_END
            && discount >= Dec::new(5, 2, 2)
            && discount <= Dec::new(7, 2, 2)
            && lineitem_quantity(line.l_quantity) < Dec::integer(24))
        .then_some(Dec::new(i128::from(line.l_extendedprice), 15, 2).mul(discount))
    }));

    vec![vec![match revenue {
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
        return Err("Q6 coverage requires a non-empty expected result".to_string());
    }
    if result[0].is_empty() || matches!(result[0][0], Value::Null) {
        return Err("Q6 coverage requires a non-NULL aggregate value".to_string());
    }
    Ok(())
}
