use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec {
    pub value: i128,
    pub precision: u8,
    pub scale: u8,
}

impl Dec {
    pub const fn new(value: i128, precision: u8, scale: u8) -> Self {
        Self {
            value,
            precision,
            scale,
        }
    }

    pub const fn integer(value: i128) -> Self {
        Self::new(value, 1, 0)
    }

    pub fn add(self, other: Self) -> Self {
        let scale = self.scale.max(other.scale);
        let left = self.value * power10(scale - self.scale);
        let right = other.value * power10(scale - other.scale);
        Self::new(left + right, self.precision.max(other.precision), scale)
    }

    pub fn sub(self, other: Self) -> Self {
        self.add(Self::new(-other.value, other.precision, other.scale))
    }

    pub fn mul(self, other: Self) -> Self {
        Self::new(
            self.value * other.value,
            self.precision.saturating_add(other.precision).min(18),
            self.scale.saturating_add(other.scale),
        )
    }

    pub fn div(self, other: Self) -> Self {
        assert_ne!(other.value, 0, "decimal division by zero");
        let scale = self.scale.saturating_add(4);
        let exponent = i32::from(scale) + i32::from(other.scale) - i32::from(self.scale);
        let numerator = if exponent >= 0 {
            self.value * power10(exponent as u8)
        } else {
            self.value / power10((-exponent) as u8)
        };
        Self::new(round_half_away_from_zero(numerator, other.value), 18, scale)
    }

    pub fn sum(values: impl IntoIterator<Item = Self>) -> Option<Self> {
        let mut values = values.into_iter();
        let first = values.next()?;
        Some(
            values
                .fold(first, |total, value| total.add(value))
                .with_precision(18),
        )
    }

    pub fn avg(values: impl IntoIterator<Item = Self>) -> Option<Self> {
        let values: Vec<_> = values.into_iter().collect();
        let first = *values.first()?;
        let total = Self::sum(values.iter().copied())?;
        let divisor = Self::integer(values.len() as i128);
        Some(
            total
                .div(divisor)
                .rescale(first.scale.saturating_add(4))
                .with_precision(18),
        )
    }

    pub const fn with_precision(mut self, precision: u8) -> Self {
        self.precision = precision;
        self
    }

    pub fn rescale(self, scale: u8) -> Self {
        if scale >= self.scale {
            Self::new(
                self.value * power10(scale - self.scale),
                self.precision,
                scale,
            )
        } else {
            Self::new(
                round_half_away_from_zero(self.value, power10(self.scale - scale)),
                self.precision,
                scale,
            )
        }
    }
}

fn power10(exponent: u8) -> i128 {
    10_i128.pow(u32::from(exponent))
}

fn round_half_away_from_zero(numerator: i128, denominator: i128) -> i128 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder == 0 {
        return quotient;
    }

    let same_sign = (numerator < 0) == (denominator < 0);
    if remainder.unsigned_abs() * 2 >= denominator.unsigned_abs() {
        quotient + if same_sign { 1 } else { -1 }
    } else {
        quotient
    }
}

impl PartialOrd for Dec {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let scale = self.scale.max(other.scale);
        let left = self.value * power10(scale - self.scale);
        let right = other.value * power10(scale - other.scale);
        Some(left.cmp(&right))
    }
}

impl fmt::Display for Dec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.value < 0;
        let absolute = self.value.unsigned_abs();
        let divisor = 10_u128.pow(u32::from(self.scale));
        let sign = if negative { "-" } else { "" };

        if self.scale == 0 {
            write!(formatter, "{sign}{absolute}")
        } else {
            write!(
                formatter,
                "{sign}{}.{:0width$}",
                absolute / divisor,
                absolute % divisor,
                width = usize::from(self.scale)
            )
        }
    }
}
