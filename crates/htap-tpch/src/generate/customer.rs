//! CUSTOMER table generation for TPC-H Clause 4.2.3.

use crate::scale_factor::{scale_factor, ScaleFactorError};

use super::rng::RandomState;
use super::text::generate_text;
use super::Customer;

/// TPC-H Clause 4.2.2.13 market-segment domain.
const MARKET_SEGMENTS: [&str; 5] = [
    "AUTOMOBILE",
    "BUILDING",
    "FURNITURE",
    "HOUSEHOLD",
    "MACHINERY",
];

/// Generates all CUSTOMER rows for the requested scale factor.
pub fn generate(
    rng: &mut RandomState,
    scale_factor_text: &str,
) -> Result<Vec<Customer>, ScaleFactorError> {
    let count = scale_factor(scale_factor_text, 150_000)?;
    let mut customers = Vec::with_capacity(count as usize);

    for custkey in 1..=count {
        let nationkey = rng.structural_bounded(25) as i64;
        customers.push(Customer {
            c_custkey: custkey as i64,
            c_name: format!("Customer#{custkey:09}"),
            c_address: generate_text(rng, 10, 40),
            c_nationkey: nationkey,
            c_phone: phone_number(rng, nationkey),
            c_acctbal: rng.two_decimal_range(-99_999, 999_999),
            c_mktsegment: MARKET_SEGMENTS
                [rng.structural_bounded(MARKET_SEGMENTS.len() as u64) as usize]
                .to_owned(),
            c_comment: generate_text(rng, 29, 116),
        });
    }

    Ok(customers)
}

/// Generates the Clause 4.2.2.9 telephone-number representation.
fn phone_number(rng: &mut RandomState, nationkey: i64) -> String {
    let country_code = nationkey + 10;
    let first = rng.structural_bounded(900) + 100;
    let second = rng.structural_bounded(900) + 100;
    let third = rng.structural_bounded(9_000) + 1_000;

    format!("{country_code:02}-{first:03}-{second:03}-{third:04}")
}

#[cfg(test)]
mod tests {
    use super::generate;
    use crate::generate::rng::RandomState;

    #[test]
    fn customer_count_matches_scale_factor() {
        let mut rng = RandomState::new(42);
        let customers = generate(&mut rng, "0.01").unwrap();

        assert_eq!(customers.len(), 1_500);
    }

    #[test]
    fn customer_keys_and_domains_are_valid() {
        let mut rng = RandomState::new(42);
        let customers = generate(&mut rng, "0.01").unwrap();

        for (index, customer) in customers.iter().enumerate() {
            assert_eq!(customer.c_custkey, index as i64 + 1);
            assert_eq!(customer.c_name, format!("Customer#{:09}", index + 1));
            assert!((0..25).contains(&customer.c_nationkey));
            assert!((10..=40).contains(&customer.c_address.len()));
            assert!((29..=116).contains(&customer.c_comment.len()));
            assert!(matches!(
                customer.c_mktsegment.as_str(),
                "AUTOMOBILE" | "BUILDING" | "FURNITURE" | "HOUSEHOLD" | "MACHINERY"
            ));
            assert!(customer
                .c_phone
                .starts_with(&format!("{:02}-", customer.c_nationkey + 10)));
        }
    }
}
