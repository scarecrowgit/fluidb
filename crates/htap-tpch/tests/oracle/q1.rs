use std::collections::BTreeMap;

use htap_common::types::Value;

use super::dates::Q1_SHIPDATE_CUTOFF;
use super::decimal::Dec;
use super::lineitem_quantity;

pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    #[derive(Default)]
    struct Group {
        quantities: Vec<Dec>,
        prices: Vec<Dec>,
        discounts: Vec<Dec>,
        discounted_prices: Vec<Dec>,
        charges: Vec<Dec>,
    }

    let mut groups = BTreeMap::<(String, String), Group>::new();
    for line in dataset
        .lineitem
        .iter()
        .filter(|line| line.l_shipdate.as_str() <= Q1_SHIPDATE_CUTOFF)
    {
        let quantity = lineitem_quantity(line.l_quantity);
        let price = Dec::new(i128::from(line.l_extendedprice), 15, 2);
        let discount = Dec::new(i128::from(line.l_discount), 15, 2);
        let tax = Dec::new(i128::from(line.l_tax), 15, 2);
        let discounted_price = price.mul(Dec::integer(1).sub(discount));
        let charge = discounted_price.mul(Dec::integer(1).add(tax));

        let group = groups
            .entry((line.l_returnflag.clone(), line.l_linestatus.clone()))
            .or_default();
        group.quantities.push(quantity);
        group.prices.push(price);
        group.discounts.push(discount);
        group.discounted_prices.push(discounted_price);
        group.charges.push(charge);
    }

    groups
        .into_iter()
        .map(|((return_flag, line_status), group)| {
            // AVG rounds once after dividing SUM by the group cardinality.
            let quantity_sum = Dec::sum(group.quantities.iter().copied()).unwrap();
            let price_sum = Dec::sum(group.prices.iter().copied()).unwrap();
            let discounted_sum = Dec::sum(group.discounted_prices.iter().copied()).unwrap();
            let charge_sum = Dec::sum(group.charges.iter().copied()).unwrap();
            let average_quantity = Dec::avg(group.quantities.iter().copied()).unwrap();
            let average_price = Dec::avg(group.prices.iter().copied()).unwrap();
            let average_discount = Dec::avg(group.discounts.iter().copied()).unwrap();

            vec![
                Value::String(return_flag),
                Value::String(line_status),
                value(quantity_sum),
                value(price_sum),
                value(discounted_sum),
                value(charge_sum),
                value(average_quantity),
                value(average_price),
                value(average_discount),
                Value::Int64(group.quantities.len() as i64),
            ]
        })
        .collect()
}

pub fn verify_coverage(dataset: &htap_tpch::Dataset) -> Result<(), String> {
    if expected(dataset).is_empty() {
        Err("Q1 has no expected results for this dataset".to_string())
    } else {
        Ok(())
    }
}

fn value(decimal: Dec) -> Value {
    Value::Decimal {
        value: decimal.value as i64,
        precision: decimal.precision,
        scale: decimal.scale,
    }
}
