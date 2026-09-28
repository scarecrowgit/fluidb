//! Incomplete ORDERS shell generation for TPC-H Clause 4.2.3.
//!
//! `OrderShell` deliberately excludes `O_ORDERSTATUS` and `O_TOTALPRICE`.
//! B2c-2 must derive those columns from the generated LINEITEM rows before an
//! order can be materialized as the public `Orders` table row.

use std::collections::HashSet;

use crate::scale_factor::{scale_factor, ScaleFactorError};

use super::rng::RandomState;
use super::text::{generate_text, generate_text_with_phrase};

/// An ORDERS row before lineitem-derived fields can be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderShell {
    pub o_orderkey: i64,
    pub o_custkey: i64,
    pub o_orderdate: String,
    pub o_orderpriority: String,
    pub o_clerk: String,
    pub o_shippriority: i32,
    pub o_comment: String,
}

/// TPC-H Clause 4.2.2.13 order-priority domain.
const ORDER_PRIORITIES: [&str; 5] = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];

/// Query 13 Clause 2.4.13.3 substitution parameters.
pub const QUERY_13_WORD1: [&str; 4] = ["special", "pending", "unusual", "express"];
pub const QUERY_13_WORD2: [&str; 4] = ["packages", "requests", "accounts", "deposits"];

/// The fraction of orders whose comments receive a Query 13 word pair.
///
/// This rate and its cohort-selection mechanism are this generator's addition,
/// not a rate prescribed by the TPC-H specification.
const FORCED_COMMENT_DIVISOR: u64 = 10;

/// Returns customer keys eligible to receive ORDERS rows.
///
/// Customer keys divisible by three are excluded structurally, so they receive
/// exactly zero orders.
pub(crate) fn eligible_customers(customer_count: u64) -> Vec<u64> {
    (1..=customer_count)
        .filter(|custkey| custkey % 3 != 0)
        .collect()
}

/// Assembles an ORDERS shell while preserving the generator's field draw order.
pub(crate) fn assemble_order_shell(
    rng: &mut RandomState,
    scale_factor_text: &str,
    orderkey: u64,
    custkey: u64,
    comment: String,
) -> Result<OrderShell, ScaleFactorError> {
    Ok(OrderShell {
        o_orderkey: orderkey as i64,
        o_custkey: custkey as i64,
        o_orderdate: order_date(rng),
        o_orderpriority: ORDER_PRIORITIES
            [rng.structural_bounded(ORDER_PRIORITIES.len() as u64) as usize]
            .to_owned(),
        o_clerk: format!(
            "Clerk#{:09}",
            rng.structural_bounded(scale_factor(scale_factor_text, 1_000)?) + 1
        ),
        o_shippriority: 0,
        o_comment: comment,
    })
}

/// Generates incomplete ORDERS shells for the requested scale factor.
///
/// There are ten orders for every CUSTOMER row. Customer keys divisible by
/// three are excluded structurally, so they are assigned exactly zero orders.
pub fn generate(
    rng: &mut RandomState,
    scale_factor_text: &str,
) -> Result<Vec<OrderShell>, ScaleFactorError> {
    let customer_count = scale_factor(scale_factor_text, 150_000)?;
    let order_count = scale_factor(scale_factor_text, 1_500_000)?;
    let forced_comment_count = order_count / FORCED_COMMENT_DIVISOR;
    let forced_comment_keys = select_forced_comment_keys(rng, order_count, forced_comment_count);

    let eligible_customers = eligible_customers(customer_count);
    assert!(
        !eligible_customers.is_empty(),
        "at least one customer must be eligible for orders"
    );

    let mut orders = Vec::with_capacity(order_count as usize);
    for order_index in 0..order_count {
        let orderkey = sparse_order_key(order_index);
        let custkey =
            eligible_customers[rng.structural_bounded(eligible_customers.len() as u64) as usize];
        let comment = if forced_comment_keys.contains(&order_index) {
            let word1 = QUERY_13_WORD1[rng.text_bounded(QUERY_13_WORD1.len() as u64) as usize];
            let word2 = QUERY_13_WORD2[rng.text_bounded(QUERY_13_WORD2.len() as u64) as usize];
            generate_text_with_phrase(rng, word1, word2, 19, 78)
        } else {
            generate_text(rng, 19, 78)
        };

        orders.push(assemble_order_shell(
            rng,
            scale_factor_text,
            orderkey,
            custkey,
            comment,
        )?);
    }

    Ok(orders)
}

/// Selects a random, disjoint cohort before order rows are generated, keeping
/// phrase assignment independent of each row's customer-key and date draws.
fn select_forced_comment_keys(
    rng: &mut RandomState,
    order_count: u64,
    forced_comment_count: u64,
) -> HashSet<u64> {
    let mut indices: Vec<u64> = (0..order_count).collect();

    for index in 0..forced_comment_count {
        let selected = index + rng.structural_bounded(order_count - index);
        indices.swap(index as usize, selected as usize);
    }

    indices[..forced_comment_count as usize]
        .iter()
        .copied()
        .collect()
}

/// Maps a dense zero-based row index into one of four sparse key slices.
pub(crate) fn sparse_order_key_in_slice(order_index: u64, slice: u64) -> u64 {
    (order_index / 8) * 32 + (order_index % 8) + 1 + 8 * slice
}

/// Maps a dense zero-based row index to the sparse key space required by
/// Clause 4.2.3:3991-3996: only keys 1 through 8 of each group of 32 are used.
fn sparse_order_key(order_index: u64) -> u64 {
    sparse_order_key_in_slice(order_index, 0)
}

/// Returns a date uniformly chosen from STARTDATE through ENDDATE - 151 days.
///
/// The date window is computed from the Clause 4.2.2.12 endpoints rather than
/// hardcoding its derived final date.
fn order_date(rng: &mut RandomState) -> String {
    const START_YEAR: i32 = 1992;
    const START_MONTH: u32 = 1;
    const START_DAY: u32 = 1;
    const END_YEAR: i32 = 1998;
    const END_MONTH: u32 = 12;
    const END_DAY: u32 = 31;
    const ORDER_DATE_OFFSET_FROM_END: i64 = 151;

    let start_days = days_from_civil(START_YEAR, START_MONTH, START_DAY);
    let end_days = days_from_civil(END_YEAR, END_MONTH, END_DAY) - ORDER_DATE_OFFSET_FROM_END;
    civil_from_days(start_days + rng.structural_bounded((end_days - start_days + 1) as u64) as i64)
}

/// Converts a Gregorian date to days since 1970-01-01.
pub(super) fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
}

/// Converts days since 1970-01-01 to a zero-padded Gregorian date string.
pub(super) fn civil_from_days(days: i64) -> String {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_prime = (5 * doy + 2) / 153;
    let day = doy - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        civil_from_days, days_from_civil, generate, order_date, sparse_order_key,
        sparse_order_key_in_slice, OrderShell, QUERY_13_WORD1, QUERY_13_WORD2,
    };
    use crate::generate::rng::RandomState;

    #[test]
    fn order_count_matches_scale_factor() {
        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();

        assert_eq!(orders.len(), 15_000);
    }

    #[test]
    fn excluded_customers_have_exactly_zero_orders() {
        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();
        let orders_by_customer: BTreeMap<i64, usize> =
            orders.iter().fold(BTreeMap::new(), |mut counts, order| {
                *counts.entry(order.o_custkey).or_default() += 1;
                counts
            });

        for custkey in 1..=1_500 {
            if custkey % 3 == 0 {
                assert_eq!(orders_by_customer.get(&custkey), None);
            }
        }
        assert!(orders.iter().all(|order| order.o_custkey % 3 != 0));
    }

    #[test]
    fn order_customers_resolve_to_generated_customer_keys() {
        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();

        assert!(orders
            .iter()
            .all(|order| (1..=1_500).contains(&order.o_custkey)));
    }

    #[test]
    fn sparse_order_keys_are_unique_and_follow_first_eight_of_thirty_two() {
        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();
        let keys: BTreeSet<i64> = orders.iter().map(|order| order.o_orderkey).collect();

        assert_eq!(keys.len(), orders.len());
        assert!(orders.iter().all(|order| {
            let offset = order.o_orderkey % 32;
            (1..=8).contains(&offset)
        }));
        assert!(orders
            .iter()
            .all(|order| order.o_orderkey < (orders.len() * 4) as i64));
    }

    #[test]
    fn order_date_window_uses_both_specification_boundaries() {
        let start = days_from_civil(1992, 1, 1);
        let end = days_from_civil(1998, 12, 31) - 151;
        assert_eq!(civil_from_days(start), "1992-01-01");
        assert_eq!(civil_from_days(end), "1998-08-02");

        let mut rng = RandomState::new(42);
        for _ in 0..10_000 {
            let date = order_date(&mut rng);
            assert!(("1992-01-01"..="1998-08-02").contains(&date.as_str()));
        }
    }

    #[test]
    fn forced_comment_phrase_has_documented_rate_and_varied_pairs() {
        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();
        let forced: Vec<&OrderShell> = orders
            .iter()
            .filter(|order| {
                [
                    "special packages",
                    "special requests",
                    "special accounts",
                    "special deposits",
                    "pending packages",
                    "pending requests",
                    "pending accounts",
                    "pending deposits",
                    "unusual packages",
                    "unusual requests",
                    "unusual accounts",
                    "unusual deposits",
                    "express packages",
                    "express requests",
                    "express accounts",
                    "express deposits",
                ]
                .iter()
                .any(|phrase| order.o_comment.contains(phrase))
            })
            .collect();

        assert_eq!(forced.len(), orders.len() / 10);
        let pairs: BTreeSet<&str> = forced
            .iter()
            .flat_map(|order| {
                [
                    "special packages",
                    "special requests",
                    "special accounts",
                    "special deposits",
                    "pending packages",
                    "pending requests",
                    "pending accounts",
                    "pending deposits",
                    "unusual packages",
                    "unusual requests",
                    "unusual accounts",
                    "unusual deposits",
                    "express packages",
                    "express requests",
                    "express accounts",
                    "express deposits",
                ]
                .into_iter()
                .filter(move |phrase| order.o_comment.contains(phrase))
            })
            .collect();
        assert!(pairs.len() > 1);
    }

    #[test]
    fn forced_phrase_cohort_is_not_customer_key_derived() {
        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();
        let forced_customer_keys: BTreeSet<i64> = orders
            .iter()
            .filter(|order| {
                QUERY_13_WORD1.iter().any(|word1| {
                    QUERY_13_WORD2
                        .iter()
                        .any(|word2| order.o_comment.contains(&format!("{word1} {word2}")))
                })
            })
            .map(|order| order.o_custkey)
            .collect();

        assert!(forced_customer_keys.len() > 1);
        assert!(forced_customer_keys.iter().any(|key| key % 2 == 0));
        assert!(forced_customer_keys.iter().any(|key| key % 2 != 0));
    }

    #[test]
    fn sparse_slice_zero_matches_existing_sparse_key_mapping() {
        for index in 0..1_000 {
            assert_eq!(sparse_order_key_in_slice(index, 0), sparse_order_key(index));
        }
    }

    #[test]
    fn sparse_order_key_slices_do_not_collide_for_the_same_index() {
        for index in 0..1_000 {
            let keys: BTreeSet<_> = (0..4)
                .map(|slice| sparse_order_key_in_slice(index, slice))
                .collect();
            assert_eq!(keys.len(), 4);
        }
    }

    #[test]
    fn distinct_indices_do_not_collide_within_a_sparse_slice() {
        for slice in [0, 1, 2, 3] {
            let keys: BTreeSet<_> = (0..1_000)
                .map(|index| sparse_order_key_in_slice(index, slice))
                .collect();
            assert_eq!(keys.len(), 1_000);
        }
    }

    #[test]
    fn order_priorities_match_the_specification_domain() {
        assert_eq!(
            super::ORDER_PRIORITIES,
            ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW",]
        );

        let mut rng = RandomState::new(42);
        let orders = generate(&mut rng, "0.01").unwrap();

        assert!(orders
            .iter()
            .all(|order| { super::ORDER_PRIORITIES.contains(&order.o_orderpriority.as_str()) }));
    }
}
