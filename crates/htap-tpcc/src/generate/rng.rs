//! Deterministic random streams used by TPC-C data generation.
//!
//! TPC-C does not prescribe a row-data PRNG. This uses SplitMix64, following
//! the deterministic stream pattern used by the workspace's TPC-H generator.

/// Deterministic random state with independent structural and text sub-streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RandomState {
    structural: Splitmix64,
    text: Splitmix64,
}

impl RandomState {
    /// Creates independent sub-streams deterministically derived from `seed`.
    pub fn new(seed: u64) -> Self {
        let mut seeder = Splitmix64::new(seed);
        Self {
            structural: Splitmix64::new(seeder.next_u64()),
            text: Splitmix64::new(seeder.next_u64()),
        }
    }

    /// Returns the next structural random value.
    pub fn structural_u64(&mut self) -> u64 {
        self.structural.next_u64()
    }

    /// Returns the next text random value.
    pub fn text_u64(&mut self) -> u64 {
        self.text.next_u64()
    }

    /// Draws uniformly from `0..bound` using rejection sampling.
    pub fn structural_bounded(&mut self, bound: u64) -> u64 {
        bounded_draw(&mut self.structural, bound)
    }

    /// Draws uniformly from `0..bound` using rejection sampling.
    pub fn text_bounded(&mut self, bound: u64) -> u64 {
        bounded_draw(&mut self.text, bound)
    }

    /// Draws a two-decimal-place amount from the inclusive scaled-integer range.
    pub fn two_decimal_range(&mut self, min: i64, max: i64) -> i64 {
        assert!(min <= max, "minimum amount must not exceed maximum");
        let width = (max as i128 - min as i128 + 1) as u64;
        min + self.structural_bounded(width) as i64
    }

    /// Draws a four-decimal-place amount from the inclusive scaled-integer range.
    pub fn four_decimal_range(&mut self, min: i64, max: i64) -> i64 {
        assert!(min <= max, "minimum amount must not exceed maximum");
        let width = (max as i128 - min as i128 + 1) as u64;
        min + self.structural_bounded(width) as i64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Splitmix64 {
    state: u64,
}

impl Splitmix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}

fn bounded_draw(stream: &mut Splitmix64, bound: u64) -> u64 {
    assert!(bound != 0, "bounded draw requires a nonzero bound");

    let threshold = bound.wrapping_neg() % bound;
    loop {
        let value = stream.next_u64();
        if value >= threshold {
            return value % bound;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RandomState;

    #[test]
    fn fixed_seed_is_deterministic() {
        let mut first = RandomState::new(42);
        let mut second = RandomState::new(42);

        for _ in 0..100 {
            assert_eq!(first.structural_u64(), second.structural_u64());
            assert_eq!(first.text_u64(), second.text_u64());
        }
    }

    #[test]
    fn decimal_range_is_inclusive() {
        let mut rng = RandomState::new(7);
        for _ in 0..1_000 {
            assert!((125..=275).contains(&rng.two_decimal_range(125, 275)));
        }
    }
}
