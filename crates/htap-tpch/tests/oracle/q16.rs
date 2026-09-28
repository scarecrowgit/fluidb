use std::collections::{BTreeMap, BTreeSet};

use htap_common::types::Value;

/// Q16 result types: p_brand String, p_type String, p_size Int64, supplier_cnt Int64.
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    let complaint_suppliers: BTreeSet<_> = dataset
        .supplier
        .iter()
        // s_comment is never NULL, so NOT IN is an anti-join here.
        .filter(|supplier| {
            supplier.s_comment.find("Customer").is_some_and(|start| {
                supplier.s_comment[start + "Customer".len()..].contains("Complaints")
            })
        })
        .map(|supplier| supplier.s_suppkey)
        .collect();

    let parts: BTreeMap<_, _> = dataset
        .part
        .iter()
        .filter(|part| {
            part.p_brand != "Brand#45"
                && !part.p_type.starts_with("MEDIUM POLISHED")
                && matches!(part.p_size, 49 | 14 | 23 | 45 | 19 | 3 | 36 | 9)
        })
        .map(|part| (part.p_partkey, part))
        .collect();

    let mut groups = BTreeMap::<(String, String, i32), BTreeSet<i64>>::new();
    for supply in &dataset.partsupp {
        let Some(part) = parts.get(&supply.ps_partkey) else {
            continue;
        };
        if complaint_suppliers.contains(&supply.ps_suppkey) {
            continue;
        }

        groups
            .entry((part.p_brand.clone(), part.p_type.clone(), part.p_size))
            .or_default()
            .insert(supply.ps_suppkey);
    }

    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|((brand, part_type, size), suppliers)| {
            vec![
                Value::String(brand),
                Value::String(part_type),
                Value::Int32(size),
                Value::Int64(suppliers.len() as i64),
            ]
        })
        .collect();
    rows.sort_by(|left, right| {
        integer(&right[3])
            .cmp(&integer(&left[3]))
            .then_with(|| string(&left[0]).cmp(string(&right[0])))
            .then_with(|| string(&left[1]).cmp(string(&right[1])))
            .then_with(|| integer(&left[2]).cmp(&integer(&right[2])))
    });
    rows
}

fn string(value: &Value) -> &str {
    match value {
        Value::String(value) => value,
        _ => unreachable!(),
    }
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::Int64(value) => *value,
        Value::Int32(value) => i64::from(*value),
        _ => unreachable!(),
    }
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    let has_complaint_supplier_pair = dataset.partsupp.iter().any(|supply| {
        let supplier_has_complaints = dataset
            .supplier
            .iter()
            .find(|supplier| supplier.s_suppkey == supply.ps_suppkey)
            .is_some_and(|supplier| {
                supplier.s_comment.find("Customer").is_some_and(|start| {
                    supplier.s_comment[start + "Customer".len()..].contains("Complaints")
                })
            });
        let part_matches_group_filter = dataset
            .part
            .iter()
            .find(|part| part.p_partkey == supply.ps_partkey)
            .is_some_and(|part| {
                part.p_brand != "Brand#45"
                    && !part.p_type.starts_with("MEDIUM POLISHED")
                    && matches!(part.p_size, 49 | 14 | 23 | 45 | 19 | 3 | 36 | 9)
            });

        supplier_has_complaints && part_matches_group_filter
    });

    has_complaint_supplier_pair.then_some(()).ok_or_else(|| {
        "coverage requires a complaint supplier that supplies a group-filtered part".to_string()
    })
}
