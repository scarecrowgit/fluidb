use crate::scale_factor::{scale_factor, ScaleFactorError};

use super::rng::RandomState;
use super::text::generate_text;
use super::{supplier_key, Partsupp};

/// Generates all PARTSUPP rows for the requested scale factor.
pub fn generate(
    rng: &mut RandomState,
    scale_factor_text: &str,
    part_count: u64,
    supplier_count: u64,
) -> Result<Vec<Partsupp>, ScaleFactorError> {
    let count = scale_factor(scale_factor_text, 800_000)?;
    let mut partsupp = Vec::with_capacity(count as usize);

    for partkey in 1..=part_count {
        for supplier_offset in 0..4_u64 {
            let suppkey = supplier_key(partkey, supplier_offset, supplier_count);

            partsupp.push(Partsupp {
                ps_partkey: partkey as i64,
                ps_suppkey: suppkey as i64,
                ps_availqty: (rng.structural_bounded(9_999) + 1) as i32,
                ps_supplycost: rng.two_decimal_range(100, 100_000),
                ps_comment: generate_text(rng, 49, 198),
            });
        }
    }

    debug_assert_eq!(partsupp.len(), count as usize);
    Ok(partsupp)
}
