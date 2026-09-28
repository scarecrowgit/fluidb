//! HISTORY surrogate key construction.

const SEQUENCE_BITS: u32 = 48;
const MAX_SEQUENCE: u64 = (1_u64 << SEQUENCE_BITS) - 1;
const MAX_SOURCE: u16 = (1_u16 << 15) - 1;

/// Builds a positive signed HISTORY key with a 15-bit source and 48-bit sequence.
///
/// The high 15 bits identify the source: 0 is reserved for initial population,
/// while runtime callers use a terminal or session number. The low 48 bits are
/// that source's monotonically increasing sequence. Restricting `source` to 15
/// bits ensures the resulting value always fits in the schema's signed BIGINT.
pub fn build_h_id(source: u16, sequence: u64) -> Result<i64, &'static str> {
    if source > MAX_SOURCE {
        return Err("history source must fit in 15 bits");
    }
    if sequence > MAX_SEQUENCE {
        return Err("history sequence must fit in 48 bits");
    }

    Ok(((u64::from(source) << SEQUENCE_BITS) | sequence) as i64)
}

#[cfg(test)]
mod tests {
    use super::build_h_id;

    const SEQUENCE_LIMIT: u64 = 1_u64 << 48;

    #[test]
    fn distinct_source_sequence_pairs_produce_distinct_ids() {
        let first = build_h_id(1, 42).unwrap();
        let second = build_h_id(2, 42).unwrap();
        let third = build_h_id(1, 43).unwrap();

        assert_ne!(first, second);
        assert_ne!(first, third);
        assert_ne!(second, third);
    }

    #[test]
    fn rejects_out_of_range_values() {
        assert!(build_h_id(0, SEQUENCE_LIMIT).is_err());
        assert!(build_h_id(1 << 15, 0).is_err());
    }

    #[test]
    fn accepts_boundary_values() {
        assert_eq!(build_h_id(0, 0), Ok(0));
        assert_eq!(
            build_h_id(0, SEQUENCE_LIMIT - 1),
            Ok((SEQUENCE_LIMIT - 1) as i64)
        );
        assert_eq!(build_h_id((1 << 15) - 1, SEQUENCE_LIMIT - 1), Ok(i64::MAX));
    }
}
