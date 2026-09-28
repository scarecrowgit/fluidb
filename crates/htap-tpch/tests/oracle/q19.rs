use std::collections::BTreeMap;

use htap_common::types::Value;

use super::decimal::Dec;
use super::lineitem_quantity;

/// Q19 result type: revenue DECIMAL(18,4).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_extendedprice: DECIMAL(15,2); l_discount: DECIMAL(15,2).
    // 1 - l_discount: DECIMAL(15,2).
    // l_extendedprice * (1 - l_discount): DECIMAL(18,4).
    // SUM(revenue): DECIMAL(18,4); no rounding occurs.
    let parts: BTreeMap<_, _> = dataset
        .part
        .iter()
        .map(|part| (part.p_partkey, part))
        .collect();

    let revenues = dataset.lineitem.iter().filter_map(|line| {
        let part = parts[&line.l_partkey];
        let quantity = lineitem_quantity(line.l_quantity);
        // The generator emits REG AIR, not AIR REG; per the specification only AIR matches.
        let matches = line.l_shipmode == "AIR"
            && line.l_shipinstruct == "DELIVER IN PERSON"
            && ((part.p_brand == "Brand#12"
                && matches!(
                    part.p_container.as_str(),
                    "SM CASE" | "SM BOX" | "SM PACK" | "SM PKG"
                )
                && (1..=5).contains(&part.p_size)
                && quantity >= Dec::integer(1)
                && quantity <= Dec::integer(11))
                || (part.p_brand == "Brand#23"
                    && matches!(
                        part.p_container.as_str(),
                        "MED BAG" | "MED BOX" | "MED PKG" | "MED PACK"
                    )
                    && (1..=10).contains(&part.p_size)
                    && quantity >= Dec::integer(10)
                    && quantity <= Dec::integer(20))
                || (part.p_brand == "Brand#34"
                    && matches!(
                        part.p_container.as_str(),
                        "LG CASE" | "LG BOX" | "LG PACK" | "LG PKG"
                    )
                    && (1..=15).contains(&part.p_size)
                    && quantity >= Dec::integer(20)
                    && quantity <= Dec::integer(30)));

        matches.then(|| {
            Dec::new(i128::from(line.l_extendedprice), 15, 2)
                .mul(Dec::integer(1).sub(Dec::new(i128::from(line.l_discount), 15, 2)))
                .with_precision(18)
        })
    });

    let revenue = Dec::sum(revenues).unwrap_or(Dec::new(0, 18, 4));
    vec![vec![Value::Decimal {
        value: revenue.value as i64,
        precision: revenue.precision,
        scale: revenue.scale,
    }]]
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    let parts: BTreeMap<_, _> = dataset
        .part
        .iter()
        .map(|part| (part.p_partkey, part))
        .collect();

    let qualifies_without_shipmode = |line: &htap_tpch::generate::Lineitem| {
        let part = parts[&line.l_partkey];
        let quantity = lineitem_quantity(line.l_quantity);

        line.l_shipinstruct == "DELIVER IN PERSON"
            && ((part.p_brand == "Brand#12"
                && matches!(
                    part.p_container.as_str(),
                    "SM CASE" | "SM BOX" | "SM PACK" | "SM PKG"
                )
                && (1..=5).contains(&part.p_size)
                && quantity >= Dec::integer(1)
                && quantity <= Dec::integer(11))
                || (part.p_brand == "Brand#23"
                    && matches!(
                        part.p_container.as_str(),
                        "MED BAG" | "MED BOX" | "MED PKG" | "MED PACK"
                    )
                    && (1..=10).contains(&part.p_size)
                    && quantity >= Dec::integer(10)
                    && quantity <= Dec::integer(20))
                || (part.p_brand == "Brand#34"
                    && matches!(
                        part.p_container.as_str(),
                        "LG CASE" | "LG BOX" | "LG PACK" | "LG PKG"
                    )
                    && (1..=15).contains(&part.p_size)
                    && quantity >= Dec::integer(20)
                    && quantity <= Dec::integer(30)))
    };

    if !dataset
        .lineitem
        .iter()
        .any(|line| line.l_shipmode == "AIR" && qualifies_without_shipmode(line))
    {
        return Err(
            "Q19 coverage requires at least one lineitem satisfying the complete predicate"
                .to_string(),
        );
    }

    if !dataset
        .lineitem
        .iter()
        .any(|line| line.l_shipmode != "AIR" && qualifies_without_shipmode(line))
    {
        return Err(
            "Q19 coverage requires a non-AIR lineitem satisfying every predicate except ship mode"
                .to_string(),
        );
    }

    // The generator emits REG AIR, not AIR REG; per the specification AIR REG does not exist.
    if dataset
        .lineitem
        .iter()
        .any(|line| line.l_shipmode == "AIR REG")
    {
        return Err("Q19 coverage requires no lineitems with ship mode AIR REG".to_string());
    }

    let result = expected(dataset);
    if result.is_empty() {
        return Err("Q19 coverage requires a non-empty expected result".to_string());
    }
    if result[0].is_empty() || matches!(result[0][0], Value::Null) {
        return Err("Q19 coverage requires a non-NULL aggregate value".to_string());
    }

    Ok(())
}
