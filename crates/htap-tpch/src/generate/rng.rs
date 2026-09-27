//! Deterministic random streams used by TPC-H data generation.
//!
//! Structural and text draws use separate streams because, with one stream,
//! editing the text composer would reshuffle every downstream key and date draw,
//! making regressions undiagnosable. SplitMix64 is public domain, and this
//! workspace has no random-number crate dependency.

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
    ///
    /// For example, `two_decimal_range(125, 275)` represents an amount between
    /// 1.25 and 2.75. Values remain scaled integers and never use floating point.
    pub fn two_decimal_range(&mut self, min_scaled: i64, max_scaled: i64) -> i64 {
        assert!(
            min_scaled <= max_scaled,
            "minimum scaled amount must not exceed maximum"
        );

        let width = (max_scaled as i128 - min_scaled as i128 + 1) as u64;
        min_scaled + self.structural_bounded(width) as i64
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

    // Reject the short prefix so each remainder has exactly the same count.
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
    fn bounded_draw_covers_range_without_returning_bound() {
        let bound = 17;
        let mut rng = RandomState::new(0xC0FF_EE15);
        let mut seen = [false; 17];

        for _ in 0..10_000 {
            let value = rng.structural_bounded(bound);
            assert!(value < bound);
            seen[value as usize] = true;
        }

        assert!(seen.into_iter().all(|was_seen| was_seen));
    }
}
