use super::orders::{civil_from_days, days_from_civil, OrderShell};
use super::text::generate_text;
use super::{supplier_key, Lineitem, Part, Partsupp};
use crate::generate::{RandomState, ScaleFactorError};
use crate::scale_factor::scale_factor;

const SHIP_INSTRUCTS: [&str; 4] = [
    "DELIVER IN PERSON",
    "COLLECT COD",
    "NONE",
    "TAKE BACK RETURN",
];

const SHIP_MODES: [&str; 7] = ["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"];

const CURRENT_DATE: &str = "1995-06-17";

pub fn generate(
    rng: &mut RandomState,
    scale_factor_text: &str,
    orders_shells: &[OrderShell],
    parts: &[Part],
    _partsupp: &[Partsupp],
) -> Result<Vec<Lineitem>, ScaleFactorError> {
    let supplier_count = scale_factor(scale_factor_text, 10_000)? as i64;
    let mut lineitems = Vec::with_capacity(orders_shells.len() * 4);

    for shell in orders_shells {
        let line_count = rng.structural_bounded(7) + 1;

        for line_number in 1..=line_count {
            let part = &parts[rng.structural_bounded(parts.len() as u64) as usize];
            let partkey = part.p_partkey;
            let supplier_offset = rng.structural_bounded(4);
            let suppkey =
                supplier_key(partkey as u64, supplier_offset, supplier_count as u64) as i64;

            let quantity = rng.structural_bounded(50) as i64 + 1;
            let orderdate_days = parse_date(&shell.o_orderdate);
            let shipdate_days = orderdate_days + rng.structural_bounded(121) as i64 + 1;
            let commitdate_days = orderdate_days + rng.structural_bounded(61) as i64 + 30;
            let receiptdate_days = shipdate_days + rng.structural_bounded(30) as i64 + 1;

            let shipdate = civil_from_days(shipdate_days);
            let commitdate = civil_from_days(commitdate_days);
            let receiptdate = civil_from_days(receiptdate_days);

            lineitems.push(Lineitem {
                l_orderkey: shell.o_orderkey,
                l_partkey: partkey,
                l_suppkey: suppkey,
                l_linenumber: line_number as i32,
                l_quantity: quantity,
                l_extendedprice: quantity * part.p_retailprice,
                l_discount: rng.two_decimal_range(0, 10),
                l_tax: rng.two_decimal_range(0, 8),
                l_returnflag: if receiptdate.as_str() <= CURRENT_DATE {
                    if rng.structural_bounded(2) == 0 {
                        "R".to_owned()
                    } else {
                        "A".to_owned()
                    }
                } else {
                    "N".to_owned()
                },
                l_linestatus: if shipdate.as_str() > CURRENT_DATE {
                    "O".to_owned()
                } else {
                    "F".to_owned()
                },
                l_shipdate: shipdate,
                l_commitdate: commitdate,
                l_receiptdate: receiptdate,
                l_shipinstruct: SHIP_INSTRUCTS
                    [rng.structural_bounded(SHIP_INSTRUCTS.len() as u64) as usize]
                    .to_owned(),
                l_shipmode: SHIP_MODES[rng.structural_bounded(SHIP_MODES.len() as u64) as usize]
                    .to_owned(),
                l_comment: generate_text(rng, 10, 43),
            });
        }
    }

    Ok(lineitems)
}

fn parse_date(date: &str) -> i64 {
    let mut components = date.split('-');
    let year = components
        .next()
        .expect("date must include a year")
        .parse()
        .expect("date year must be numeric");
    let month = components
        .next()
        .expect("date must include a month")
        .parse()
        .expect("date month must be numeric");
    let day = components
        .next()
        .expect("date must include a day")
        .parse()
        .expect("date day must be numeric");

    assert!(
        components.next().is_none(),
        "date must use YYYY-MM-DD format"
    );

    days_from_civil(year, month, day)
}

#[cfg(test)]
mod tests {
    use super::super::tests::shared_dataset_at_0_01;
    use super::*;
    use crate::generate;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn line_count_is_within_expected_band_for_scale_point_zero_one() {
        // Scale 0.01 yields enough orders to validate line counts while avoiding
        // slower generation at scale 0.1. Generation is deterministic for this
        // seed, so a tight band does not introduce test flakiness.
        let dataset = shared_dataset_at_0_01();

        assert_eq!(dataset.orders_shells.len(), 15_000);
        assert!((15_000..=105_000).contains(&dataset.lineitem.len()));
        assert!((59_400..=60_600).contains(&dataset.lineitem.len()));
        assert!(dataset.lineitem.len() >= dataset.orders_shells.len());
        assert!(dataset.lineitem.len() <= dataset.orders_shells.len() * 7);
    }

    #[test]
    fn every_part_supplier_pair_resolves_to_partsupp() {
        let dataset = shared_dataset_at_0_01();
        let pairs: HashSet<_> = dataset
            .partsupp
            .iter()
            .map(|row| (row.ps_partkey, row.ps_suppkey))
            .collect();

        for lineitem in &dataset.lineitem {
            assert!(
                pairs.contains(&(lineitem.l_partkey, lineitem.l_suppkey)),
                "missing partsupp row for ({}, {})",
                lineitem.l_partkey,
                lineitem.l_suppkey
            );
        }
    }

    #[test]
    fn every_order_key_resolves_to_an_order() {
        let dataset = shared_dataset_at_0_01();
        let orderkeys: HashSet<_> = dataset
            .orders
            .iter()
            .map(|order| order.o_orderkey)
            .collect();

        for lineitem in &dataset.lineitem {
            assert!(orderkeys.contains(&lineitem.l_orderkey));
        }
    }

    #[test]
    fn dates_follow_tpch_relationships() {
        let dataset = shared_dataset_at_0_01();
        let order_dates: HashMap<_, _> = dataset
            .orders
            .iter()
            .map(|order| (order.o_orderkey, parse_date(&order.o_orderdate)))
            .collect();

        for lineitem in &dataset.lineitem {
            let orderdate = order_dates[&lineitem.l_orderkey];
            let shipdate = parse_date(&lineitem.l_shipdate);
            let commitdate = parse_date(&lineitem.l_commitdate);
            let receiptdate = parse_date(&lineitem.l_receiptdate);

            assert!((30..=90).contains(&(commitdate - orderdate)));
            assert!((1..=121).contains(&(shipdate - orderdate)));
            assert!((1..=30).contains(&(receiptdate - shipdate)));
        }
    }

    #[test]
    fn shipping_domains_are_exact_and_pinned() {
        let dataset = shared_dataset_at_0_01();
        let ship_instructs: HashSet<_> = dataset
            .lineitem
            .iter()
            .map(|lineitem| lineitem.l_shipinstruct.as_str())
            .collect();
        let ship_modes: HashSet<_> = dataset
            .lineitem
            .iter()
            .map(|lineitem| lineitem.l_shipmode.as_str())
            .collect();

        assert_eq!(
            ship_instructs,
            HashSet::from([
                "DELIVER IN PERSON",
                "COLLECT COD",
                "NONE",
                "TAKE BACK RETURN",
            ])
        );
        assert_eq!(
            ship_modes,
            HashSet::from(["REG AIR", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"])
        );
    }

    #[test]
    fn sample_order_totals_and_statuses_recompute() {
        let dataset = shared_dataset_at_0_01();
        let lineitems_by_order: HashMap<_, Vec<_>> =
            dataset
                .lineitem
                .iter()
                .fold(HashMap::new(), |mut by_order, lineitem| {
                    by_order
                        .entry(lineitem.l_orderkey)
                        .or_default()
                        .push(lineitem);
                    by_order
                });

        let samples: Vec<_> = dataset.orders.iter().take(5).collect();
        assert_eq!(samples.len(), 5);

        for order in samples {
            let order_lineitems = &lineitems_by_order[&order.o_orderkey];
            let total_numerator: i128 = order_lineitems
                .iter()
                .map(|lineitem| {
                    i128::from(lineitem.l_extendedprice)
                        * (100 + i128::from(lineitem.l_tax))
                        * (100 - i128::from(lineitem.l_discount))
                })
                .sum();
            let totalprice = ((total_numerator + 5_000) / 10_000) as i64;
            let status = if order_lineitems
                .iter()
                .all(|lineitem| lineitem.l_linestatus == "F")
            {
                "F"
            } else if order_lineitems
                .iter()
                .all(|lineitem| lineitem.l_linestatus == "O")
            {
                "O"
            } else {
                "P"
            };

            assert_eq!(order.o_totalprice, totalprice);
            assert_eq!(order.o_orderstatus, status);
        }
    }

    #[test]
    fn discount_tax_ranges_and_order_totals_follow_scaled_convention() {
        let dataset = shared_dataset_at_0_01();

        for lineitem in &dataset.lineitem {
            assert!((0..=10).contains(&lineitem.l_discount));
            assert!((0..=8).contains(&lineitem.l_tax));
        }

        assert!(dataset.orders.iter().all(|order| order.o_totalprice >= 0));
    }

    #[test]
    fn whole_dataset_is_deterministic() {
        assert_eq!(
            generate::generate("0.01", 1).unwrap().lineitem,
            generate::generate("0.01", 1).unwrap().lineitem
        );
    }
}
