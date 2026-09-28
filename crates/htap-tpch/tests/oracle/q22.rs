use std::collections::{BTreeMap, BTreeSet};

use htap_common::types::Value;

use super::decimal::Dec;

/// Q22 result types: cntrycode String, numcust Int64, totacctbal DECIMAL(18,2).
pub fn expected(dataset: &htap_tpch::Dataset) -> Vec<Vec<Value>> {
    const COUNTRY_CODES: [&str; 7] = ["13", "31", "23", "29", "30", "18", "17"];

    // Worksheet:
    // c_acctbal: DECIMAL(15,2).
    // 0.00: DECIMAL(3,2).
    // AVG(c_acctbal): DECIMAL(18,6); division is the rounding point.
    // SUM(c_acctbal): DECIMAL(18,2); no rounding occurs.
    let average_population = dataset.customer.iter().filter_map(|customer| {
        let code = &customer.c_phone[..2];
        let balance = Dec::new(i128::from(customer.c_acctbal), 15, 2);
        (COUNTRY_CODES.contains(&code) && balance > Dec::new(0, 3, 2)).then_some(balance)
    });
    let average = Dec::avg(average_population).expect("positive account balance population");
    let customers_with_orders: BTreeSet<_> =
        dataset.orders.iter().map(|order| order.o_custkey).collect();

    let mut groups = BTreeMap::<String, Vec<Dec>>::new();
    for customer in &dataset.customer {
        let code = &customer.c_phone[..2];
        let balance = Dec::new(i128::from(customer.c_acctbal), 15, 2);
        if COUNTRY_CODES.contains(&code)
            && balance > average
            && !customers_with_orders.contains(&customer.c_custkey)
        {
            groups.entry(code.to_string()).or_default().push(balance);
        }
    }

    groups
        .into_iter()
        .map(|(code, balances)| {
            let total = Dec::sum(balances.iter().copied()).expect("group has balances");
            vec![
                Value::String(code),
                Value::Int64(balances.len() as i64),
                Value::Decimal {
                    value: total.value as i64,
                    precision: total.precision,
                    scale: total.scale,
                },
            ]
        })
        .collect()
}

super::verify_non_empty!();
