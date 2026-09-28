use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q7_END, Q7_START};
use super::decimal::Dec;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_extendedprice: DECIMAL(15,2); l_discount: DECIMAL(15,2).
    // 1 - l_discount: DECIMAL(15,2); volume: DECIMAL(18,4).
    // CASE ELSE 0 retains the THEN branch scale, yielding DECIMAL(18,4).
    // Both SUMs are DECIMAL(18,4). The final division rounds once to
    // DECIMAL(18,8).
    let parts: BTreeMap<_, _> = dataset
        .part
        .iter()
        .map(|part| (part.p_partkey, part))
        .collect();
    let suppliers: BTreeMap<_, _> = dataset
        .supplier
        .iter()
        .map(|supplier| (supplier.s_suppkey, supplier))
        .collect();
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
        .filter(|order| {
            order.o_orderdate.as_str() >= Q7_START && order.o_orderdate.as_str() <= Q7_END
        })
        .map(|order| (order.o_orderkey, order))
        .collect();
    let customers: BTreeMap<_, _> = dataset
        .customer
        .iter()
        .map(|customer| (customer.c_custkey, customer))
        .collect();
    let nations: BTreeMap<_, _> = dataset
        .nation
        .iter()
        .map(|nation| (nation.n_nationkey, nation))
        .collect();

    let mut volumes = BTreeMap::<i64, (Vec<Dec>, Vec<Dec>)>::new();
    for line in dataset.lineitem.iter() {
        let Some(part) = parts.get(&line.l_partkey) else {
            continue;
        };
        let Some(order) = orders.get(&line.l_orderkey) else {
            continue;
        };
        if part.p_type != "ECONOMY ANODIZED STEEL" {
            continue;
        }

        let customer = customers[&order.o_custkey];
        let customer_nation = nations[&customer.c_nationkey];
        let in_america = dataset.region.iter().any(|region| {
            region.r_regionkey == customer_nation.n_regionkey && region.r_name == "AMERICA"
        });
        if !in_america {
            continue;
        }

        let supplier = suppliers[&line.l_suppkey];
        let supplier_nation = nations[&supplier.s_nationkey];
        let price = Dec::new(i128::from(line.l_extendedprice), 15, 2);
        let discount = Dec::new(i128::from(line.l_discount), 15, 2);
        let volume = price.mul(Dec::integer(1).sub(discount));
        let year = order.o_orderdate[..4].parse().expect("valid order year");
        let (brazil_volumes, all_volumes) = volumes.entry(year).or_default();
        if supplier_nation.n_name == "BRAZIL" {
            brazil_volumes.push(volume);
        } else {
            brazil_volumes.push(Dec::new(0, volume.precision, volume.scale));
        }
        all_volumes.push(volume);
    }

    volumes
        .into_iter()
        .map(|(year, (brazil_volumes, all_volumes))| {
            let brazil = Dec::sum(brazil_volumes).expect("non-empty year group");
            let total = Dec::sum(all_volumes).expect("non-empty year group");
            let share = brazil.div(total);
            vec![
                Value::Int64(year),
                Value::Decimal {
                    value: share.value as i64,
                    precision: share.precision,
                    scale: share.scale,
                },
            ]
        })
        .collect()
}

super::verify_non_empty!();
