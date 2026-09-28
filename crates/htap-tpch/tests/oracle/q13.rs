use std::collections::BTreeMap;

use htap_common::types::Value;

/// Q13 result types: c_count Int64, custdist Int64.
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let mut order_counts = BTreeMap::<i64, i64>::new();
    for order in &dataset.orders {
        if !like_special_requests(&order.o_comment) {
            *order_counts.entry(order.o_custkey).or_default() += 1;
        }
    }

    let mut distribution = BTreeMap::<i64, i64>::new();
    for customer in &dataset.customer {
        *distribution
            .entry(*order_counts.get(&customer.c_custkey).unwrap_or(&0))
            .or_default() += 1;
    }

    let mut rows: Vec<_> = distribution
        .into_iter()
        .map(|(count, customers)| vec![Value::Int64(count), Value::Int64(customers)])
        .collect();
    rows.sort_by(|left, right| {
        integer(&right[1])
            .cmp(&integer(&left[1]))
            .then_with(|| integer(&right[0]).cmp(&integer(&left[0])))
    });
    rows
}

// Equivalent to LIKE '%special%requests%' without relying on regex behavior.
fn like_special_requests(comment: &str) -> bool {
    comment
        .find("special")
        .is_some_and(|start| comment[start + "special".len()..].contains("requests"))
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        _ => unreachable!(),
    }
}

super::verify_non_empty!();
