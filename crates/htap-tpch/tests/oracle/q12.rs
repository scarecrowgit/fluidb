use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q12_END, Q12_START};

/// Q12 result types: l_shipmode String, high_line_count Int64, low_line_count Int64.
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .map(|order| (order.o_orderkey, order))
        .collect();
    let mut counts = BTreeMap::<String, (i64, i64)>::new();

    for line in &dataset.lineitem {
        if !matches!(line.l_shipmode.as_str(), "MAIL" | "SHIP")
            || line.l_commitdate >= line.l_receiptdate
            || line.l_shipdate >= line.l_commitdate
            || line.l_receiptdate.as_str() < Q12_START
            || line.l_receiptdate.as_str() >= Q12_END
        {
            continue;
        }
        let order = orders[&line.l_orderkey];
        let count = counts.entry(line.l_shipmode.clone()).or_default();
        if matches!(order.o_orderpriority.as_str(), "1-URGENT" | "2-HIGH") {
            count.0 += 1;
        } else {
            count.1 += 1;
        }
    }

    counts
        .into_iter()
        .map(|(shipmode, (high, low))| {
            vec![
                Value::String(shipmode),
                Value::Int64(high),
                Value::Int64(low),
            ]
        })
        .collect()
}

super::verify_non_empty!();
