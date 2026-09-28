use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q15_END, Q15_START};
use super::decimal::Dec;

/// Q15 result types: supplier fields plus total_revenue DECIMAL(18,4).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let mut revenues = BTreeMap::<i64, Dec>::new();
    for line in dataset
        .lineitem
        .iter()
        .filter(|line| line.l_shipdate.as_str() >= Q15_START && line.l_shipdate.as_str() < Q15_END)
    {
        let revenue = Dec::new(i128::from(line.l_extendedprice), 15, 2)
            .mul(Dec::integer(1).sub(Dec::new(i128::from(line.l_discount), 15, 2)))
            .with_precision(18);
        revenues
            .entry(line.l_suppkey)
            .and_modify(|total| *total = total.add(revenue).with_precision(18))
            .or_insert(revenue);
    }

    let Some(maximum) = revenues.values().copied().reduce(|maximum, revenue| {
        if revenue
            .partial_cmp(&maximum)
            .is_some_and(|ordering| ordering.is_gt())
        {
            revenue
        } else {
            maximum
        }
    }) else {
        return vec![];
    };
    let suppliers: BTreeMap<_, _> = dataset
        .supplier
        .iter()
        .map(|supplier| (supplier.s_suppkey, supplier))
        .collect();

    revenues
        .into_iter()
        .filter(|(_, revenue)| *revenue == maximum)
        .map(|(supplier_key, revenue)| {
            let supplier = suppliers[&supplier_key];
            vec![
                Value::Int64(supplier.s_suppkey),
                Value::String(supplier.s_name.clone()),
                Value::String(supplier.s_address.clone()),
                Value::String(supplier.s_phone.clone()),
                Value::Decimal {
                    value: revenue.value as i64,
                    precision: revenue.precision,
                    scale: revenue.scale,
                },
            ]
        })
        .collect()
}

super::verify_non_empty!();
