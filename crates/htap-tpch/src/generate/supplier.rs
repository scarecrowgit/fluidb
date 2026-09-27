use std::collections::HashSet;

use crate::scale_factor::{scale_factor, ScaleFactorError};

use super::rng::RandomState;
use super::text::{generate_text, generate_text_with_phrase};
use super::Supplier;

/// Generates all SUPPLIER rows for the requested scale factor.
pub fn generate(
    rng: &mut RandomState,
    scale_factor_text: &str,
) -> Result<Vec<Supplier>, ScaleFactorError> {
    let count = scale_factor(scale_factor_text, 10_000)?;
    let complaint_count = scale_factor(scale_factor_text, 5)?;
    let recommendation_count = scale_factor(scale_factor_text, 5)?;
    let cohort_count = complaint_count + recommendation_count;
    let mut suppkeys: Vec<_> = (1..=count).collect();

    // Partially shuffle supplier keys so the two cohorts are random and disjoint.
    for index in 0..cohort_count {
        let selected_index = index + rng.structural_bounded(count - index);
        suppkeys.swap(index as usize, selected_index as usize);
    }

    let complaint_keys: HashSet<_> = suppkeys[..complaint_count as usize]
        .iter()
        .copied()
        .collect();
    let recommendation_keys: HashSet<_> = suppkeys[complaint_count as usize..cohort_count as usize]
        .iter()
        .copied()
        .collect();

    let mut suppliers = Vec::with_capacity(count as usize);

    for suppkey in 1..=count {
        let nationkey = rng.structural_bounded(25) as i64;
        let comment = if complaint_keys.contains(&suppkey) {
            generate_text_with_phrase(rng, "Customer", "Complaints", 25, 100)
        } else if recommendation_keys.contains(&suppkey) {
            generate_text_with_phrase(rng, "Customer", "Recommends", 25, 100)
        } else {
            generate_text(rng, 25, 100)
        };

        suppliers.push(Supplier {
            s_suppkey: suppkey as i64,
            s_name: format!("Supplier#{suppkey:09}"),
            s_address: generate_text(rng, 10, 40),
            s_nationkey: nationkey,
            s_phone: phone_number(rng, nationkey),
            s_acctbal: rng.two_decimal_range(-99_999, 999_999),
            s_comment: comment,
        });
    }

    Ok(suppliers)
}

fn phone_number(rng: &mut RandomState, nationkey: i64) -> String {
    let country_code = nationkey + 10;
    let first = rng.structural_bounded(900) + 100;
    let second = rng.structural_bounded(900) + 100;
    let third = rng.structural_bounded(9_000) + 1_000;

    format!("{country_code:02}-{first:03}-{second:03}-{third:04}")
}
