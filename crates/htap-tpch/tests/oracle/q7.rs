use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::{Q7_END, Q7_START};
use super::decimal::Dec;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    // Worksheet:
    // l_extendedprice: DECIMAL(15,2); l_discount: DECIMAL(15,2).
    // 1 - l_discount: DECIMAL(15,2).
    // volume is DECIMAL(18,4); SUM(revenue) is DECIMAL(18,4).
    // No rounding occurs.
    let suppliers: BTreeMap<_, _> = dataset
        .supplier
        .iter()
        .map(|supplier| (supplier.s_suppkey, supplier))
        .collect();
    let orders: BTreeMap<_, _> = dataset
        .orders
        .iter()
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

    let mut revenues = BTreeMap::<(String, String, i64), Dec>::new();
    for line in dataset
        .lineitem
        .iter()
        .filter(|line| line.l_shipdate.as_str() >= Q7_START && line.l_shipdate.as_str() <= Q7_END)
    {
        let supplier = suppliers[&line.l_suppkey];
        let order = orders[&line.l_orderkey];
        let customer = customers[&order.o_custkey];
        let supplier_nation = nations[&supplier.s_nationkey];
        let customer_nation = nations[&customer.c_nationkey];
        let nation_pair = (&supplier_nation.n_name[..], &customer_nation.n_name[..]);
        if nation_pair != ("FRANCE", "GERMANY") && nation_pair != ("GERMANY", "FRANCE") {
            continue;
        }

        let price = Dec::new(i128::from(line.l_extendedprice), 15, 2);
        let discount = Dec::new(i128::from(line.l_discount), 15, 2);
        let volume = price.mul(Dec::integer(1).sub(discount));
        let year = line.l_shipdate[..4].parse().expect("valid shipment year");
        revenues
            .entry((
                supplier_nation.n_name.clone(),
                customer_nation.n_name.clone(),
                year,
            ))
            .and_modify(|total| *total = total.add(volume).with_precision(18))
            .or_insert(volume.with_precision(18));
    }

    revenues
        .into_iter()
        .map(|((supplier_nation, customer_nation, year), revenue)| {
            vec![
                Value::String(supplier_nation),
                Value::String(customer_nation),
                Value::Int64(year),
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
