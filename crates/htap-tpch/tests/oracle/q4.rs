use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q4_END, Q4_START};

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet: COUNT(*) returns INT64. There are no decimal arithmetic steps
    // or rounding points in this query.
    let mut counts = BTreeMap::<String, i64>::new();
    for order in dataset.orders.iter().filter(|order| {
        order.o_orderdate.as_str() >= Q4_START
            && order.o_orderdate.as_str() < Q4_END
            && dataset.lineitem.iter().any(|line| {
                line.l_orderkey == order.o_orderkey
                    && line.l_commitdate.as_str() < line.l_receiptdate.as_str()
            })
    }) {
        *counts.entry(order.o_orderpriority.clone()).or_default() += 1;
    }

    counts
        .into_iter()
        .map(|(priority, count)| vec![Value::String(priority), Value::Int64(count)])
        .collect()
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    if expected(dataset).is_empty() {
        Err("Q4 has no expected results for this dataset".to_string())
    } else {
        Ok(())
    }
}
