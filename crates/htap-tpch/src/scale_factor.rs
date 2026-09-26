use std::error::Error;
use std::fmt;

/// Errors produced while converting a scale factor into a row count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleFactorError {
    /// The supplied floating-point factor is not finite or is not positive.
    InvalidScaleFactor,
    /// The scaled count cannot be represented as an unsigned 64-bit integer.
    Overflow,
}

impl fmt::Display for ScaleFactorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScaleFactor => {
                write!(
                    formatter,
                    "scale factor must be finite and greater than zero"
                )
            }
            Self::Overflow => write!(formatter, "scaled row count exceeds u64"),
        }
    }
}

impl Error for ScaleFactorError {}

/// Computes a nonzero row count from a decimal text `scale_factor` and `multiplier`.
///
/// The decimal scale factor is parsed exactly into an integer numerator and a
/// power-of-ten denominator, then multiplied and divided with `i128`
/// intermediates. The result is rounded to the nearest integer with half values
/// rounded up. A minimum result of one is enforced so callers can preserve
/// structural invariants for small fractional scale factors. Invalid,
/// non-positive, or non-representable factors and results that cannot fit in
/// `u64` return errors rather than wrapping or panicking.
pub fn scale_factor(scale_factor: &str, multiplier: u64) -> Result<u64, ScaleFactorError> {
    let (numerator, denominator) = parse_scale_factor(scale_factor)?;
    let scaled = numerator
        .checked_mul(multiplier as i128)
        .ok_or(ScaleFactorError::Overflow)?;

    let mut rounded = scaled / denominator;
    let remainder = scaled % denominator;
    if remainder >= (denominator + 1) / 2 {
        rounded = rounded.checked_add(1).ok_or(ScaleFactorError::Overflow)?;
    }

    if rounded > u64::MAX as i128 {
        return Err(ScaleFactorError::Overflow);
    }

    Ok((rounded as u64).max(1))
}

fn parse_scale_factor(scale_factor: &str) -> Result<(i128, i128), ScaleFactorError> {
    let scale_factor = scale_factor.strip_prefix('+').unwrap_or(scale_factor);
    if scale_factor.starts_with('-') {
        return Err(ScaleFactorError::InvalidScaleFactor);
    }

    let (whole, fractional) = match scale_factor.split_once('.') {
        Some((whole, fractional))
            if !fractional.contains('.') && (!whole.is_empty() || !fractional.is_empty()) =>
        {
            (whole, fractional)
        }
        Some(_) => return Err(ScaleFactorError::InvalidScaleFactor),
        None if !scale_factor.is_empty() => (scale_factor, ""),
        None => return Err(ScaleFactorError::InvalidScaleFactor),
    };

    let mut numerator = 0_i128;
    for digit in whole.bytes().chain(fractional.bytes()) {
        if !digit.is_ascii_digit() {
            return Err(ScaleFactorError::InvalidScaleFactor);
        }

        numerator = numerator
            .checked_mul(10)
            .and_then(|value| value.checked_add((digit - b'0') as i128))
            .ok_or(ScaleFactorError::InvalidScaleFactor)?;
    }

    if numerator == 0 {
        return Err(ScaleFactorError::InvalidScaleFactor);
    }

    let mut denominator = 1_i128;
    for _ in 0..fractional.len() {
        denominator = denominator
            .checked_mul(10)
            .ok_or(ScaleFactorError::InvalidScaleFactor)?;
    }

    Ok((numerator, denominator))
}

#[cfg(test)]
mod tests {
    use super::{scale_factor, ScaleFactorError};

    #[test]
    fn test_scale_factor_exact() {
        assert_eq!(scale_factor("1.0", 10_000), Ok(10_000));
    }

    #[test]
    fn test_scale_factor_fractional() {
        assert_eq!(scale_factor("0.01", 10_000), Ok(100));
    }

    #[test]
    fn test_scale_factor_rounding_boundary() {
        assert_eq!(scale_factor("0.00005", 10_000), Ok(1));
    }

    #[test]
    fn test_scale_factor_floor() {
        assert_eq!(scale_factor("0.00001", 4), Ok(1));
    }

    #[test]
    fn test_scale_factor_overflow() {
        assert_eq!(
            scale_factor("2000000000.0", 10_000_000_000),
            Err(ScaleFactorError::Overflow)
        );
    }

    #[test]
    fn test_scale_factor_invalid_input() {
        for value in ["NaN", "inf", "-inf", "-1.0", "0.0"] {
            assert_eq!(
                scale_factor(value, 10_000),
                Err(ScaleFactorError::InvalidScaleFactor)
            );
        }
    }

    #[test]
    fn test_scale_factor_exact_vs_float_discrimination() {
        // Binary floating point represents 0.29 slightly below its decimal value,
        // so multiplying by 50 can produce 14.499999999999998 instead of 14.5.
        assert_eq!(scale_factor("0.29", 50), Ok(15));
    }
}
