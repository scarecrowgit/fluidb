//! TPC-C text-field generators.

use std::ops::RangeInclusive;

use super::rng::RandomState;

const A_STRING_ALPHABET: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~ ";

const C_LAST_SYLLABLES: [&str; 10] = [
    "BAR", "OUGHT", "ABLE", "PRI", "PRES", "ESE", "ANTI", "CALLY", "ATION", "EING",
];

/// Produces an alphanumeric-special-character string in the requested range.
pub fn a_string(rng: &mut RandomState, length_range: RangeInclusive<usize>) -> String {
    let length = choose_length(rng, length_range);
    (0..length)
        .map(|_| {
            A_STRING_ALPHABET[rng.text_bounded(A_STRING_ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

/// Produces a numeric string in the requested range.
pub fn n_string(rng: &mut RandomState, length_range: RangeInclusive<usize>) -> String {
    let length = choose_length(rng, length_range);
    (0..length)
        .map(|_| (b'0' + rng.text_bounded(10) as u8) as char)
        .collect()
}

/// Implements the TPC-C non-uniform random function.
pub fn nurand(rng: &mut RandomState, a: u64, x: u64, y: u64, c: u64) -> u64 {
    assert!(x <= y, "NURand requires x to be no greater than y");

    let random_a = rng.structural_bounded(a + 1);
    let random_xy = x + rng.structural_bounded(y - x + 1);
    ((random_a | random_xy) + c) % (y - x + 1) + x
}

/// Produces a customer last name from `n`, which must be in `0..=999`.
pub fn c_last(_rng: &mut RandomState, _c: u64, n: u64) -> String {
    assert!(n <= 999, "C_LAST number must be at most 999");

    let hundreds = (n / 100) as usize;
    let tens = ((n / 10) % 10) as usize;
    let ones = (n % 10) as usize;
    format!(
        "{}{}{}",
        C_LAST_SYLLABLES[hundreds], C_LAST_SYLLABLES[tens], C_LAST_SYLLABLES[ones]
    )
}

/// Produces a four-digit numeric prefix followed by the required ZIP suffix.
pub fn zip_code(rng: &mut RandomState) -> String {
    format!("{}11111", n_string(rng, 4..=4))
}

/// Produces a Fisher-Yates permutation of `0..n`.
pub fn permutation(rng: &mut RandomState, n: usize) -> Vec<usize> {
    let mut values: Vec<_> = (0..n).collect();

    for index in (1..n).rev() {
        let swap_index = rng.structural_bounded((index + 1) as u64) as usize;
        values.swap(index, swap_index);
    }

    values
}

fn choose_length(rng: &mut RandomState, length_range: RangeInclusive<usize>) -> usize {
    let min = *length_range.start();
    let max = *length_range.end();
    assert!(min <= max, "minimum string length must not exceed maximum");
    min + rng.text_bounded((max - min + 1) as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::{a_string, c_last, n_string, nurand, permutation, zip_code, A_STRING_ALPHABET};
    use crate::generate::rng::RandomState;

    #[test]
    fn customer_last_name_uses_required_syllables() {
        let mut rng = RandomState::new(1);
        assert_eq!(c_last(&mut rng, 0, 371), "PRICALLYOUGHT");
        assert_eq!(c_last(&mut rng, 0, 40), "BARPRESBAR");
    }

    #[test]
    fn nurand_stays_in_requested_range() {
        let mut rng = RandomState::new(2);
        for _ in 0..10_000 {
            assert!((0..=999).contains(&nurand(&mut rng, 255, 0, 999, 42)));
        }
    }

    #[test]
    fn string_generators_obey_length_and_character_rules() {
        let mut rng = RandomState::new(3);
        for _ in 0..1_000 {
            let alpha = a_string(&mut rng, 8..=16);
            assert!((8..=16).contains(&alpha.len()));
            assert!(alpha.bytes().all(|byte| A_STRING_ALPHABET.contains(&byte)));

            let numeric = n_string(&mut rng, 8..=16);
            assert!((8..=16).contains(&numeric.len()));
            assert!(numeric.bytes().all(|byte| byte.is_ascii_digit()));
        }
    }

    #[test]
    fn zip_code_has_numeric_prefix_and_required_suffix() {
        let mut rng = RandomState::new(4);
        for _ in 0..100 {
            let zip = zip_code(&mut rng);
            assert_eq!(zip.len(), 9);
            assert!(zip[..4].bytes().all(|byte| byte.is_ascii_digit()));
            assert_eq!(&zip[4..], "11111");
        }
    }

    #[test]
    fn permutation_is_a_bijection() {
        let mut rng = RandomState::new(5);
        let mut result = permutation(&mut rng, 100);
        result.sort_unstable();
        assert_eq!(result, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn text_generation_is_deterministic() {
        let mut first = RandomState::new(6);
        let mut second = RandomState::new(6);
        assert_eq!(
            a_string(&mut first, 10..=20),
            a_string(&mut second, 10..=20)
        );
        assert_eq!(
            n_string(&mut first, 10..=20),
            n_string(&mut second, 10..=20)
        );
        assert_eq!(zip_code(&mut first), zip_code(&mut second));
        assert_eq!(permutation(&mut first, 20), permutation(&mut second, 20));
    }
}
