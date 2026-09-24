//! Core relational data model: [`DataType`], [`Value`], [`ColumnDef`],
//! [`Schema`] and [`Row`].
//!
//! These types are the vocabulary shared by the row store, the column store
//! and the SQL layer, so their ordering and equality semantics are part of the
//! engine's on-disk contract (see [`crate::keycodec`]).

use crate::error::{HtapError, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Largest supported DECIMAL precision. DECIMAL values are stored as signed `i64` unscaled
/// integers, so every declared and derived precision must fit within this bound.
pub const MAX_DECIMAL_PRECISION: u8 = 18;

/// Logical type of a column.
///
/// The declaration order of the variants is significant: it defines the
/// tie-breaking order used when [`Value`]s of different types are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataType {
    /// Boolean.
    Bool,
    /// Signed 32-bit integer.
    Int32,
    /// Signed 64-bit integer.
    Int64,
    /// IEEE-754 double precision float.
    Float64,
    /// UTF-8 string.
    String,
    /// Opaque byte string.
    Bytes,
    /// Microseconds since the Unix epoch, stored as an `i64`.
    Timestamp,
    /// Fixed-point decimal with up to `precision` decimal digits and `scale`
    /// digits after the decimal point.
    Decimal {
        /// Total number of decimal digits.
        precision: u8,
        /// Number of digits after the decimal point.
        scale: u8,
    },
}

impl DataType {
    /// Validates DECIMAL precision and scale against the fixed-point storage representation.
    pub fn validate(self) -> Result<()> {
        if let DataType::Decimal { precision, scale } = self {
            if precision == 0 || precision > MAX_DECIMAL_PRECISION || scale > precision {
                return Err(HtapError::InvalidArgument(format!(
                    "invalid DECIMAL({precision},{scale}); precision must be 1..={MAX_DECIMAL_PRECISION} and scale must not exceed precision"
                )));
            }
        }
        Ok(())
    }

    /// SQL-facing name of the type.
    pub fn name(&self) -> &'static str {
        match self {
            DataType::Bool => "bool",
            DataType::Int32 => "int",
            DataType::Int64 => "bigint",
            DataType::Float64 => "double",
            DataType::String => "varchar",
            DataType::Bytes => "varbinary",
            DataType::Timestamp => "timestamp",
            DataType::Decimal { .. } => "decimal",
        }
    }

    /// Discriminant rank, used as the deterministic tie-breaker when comparing
    /// two non-null values of different types.
    #[inline]
    fn rank(self) -> u8 {
        match self {
            DataType::Bool => 0,
            DataType::Int32 => 1,
            DataType::Int64 => 2,
            DataType::Float64 => 3,
            DataType::String => 4,
            DataType::Bytes => 5,
            DataType::Timestamp => 6,
            DataType::Decimal { .. } => 7,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Parses an ISO 8601 calendar date (`YYYY-MM-DD`) into UTC-midnight
/// microseconds since the Unix epoch.
pub fn parse_date_to_timestamp_micros(date: &str) -> Result<i64> {
    let invalid = || HtapError::InvalidArgument(format!("invalid calendar date: '{date}'"));

    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(invalid());
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return Err(invalid());
    }

    let year: i32 = date[0..4].parse().map_err(|_| invalid())?;
    let month: u32 = date[5..7].parse().map_err(|_| invalid())?;
    let day: u32 = date[8..10].parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&month) {
        return Err(invalid());
    }

    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => unreachable!(),
    };
    if day == 0 || day > days_in_month {
        return Err(invalid());
    }

    // Howard Hinnant's civil-date algorithm, relative to 1970-01-01.
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    days.checked_mul(86_400_000_000).ok_or_else(invalid)
}

/// Formats UTC-midnight microseconds since the Unix epoch as an ISO 8601
/// calendar date (`YYYY-MM-DD`).
pub fn timestamp_micros_to_date(timestamp_micros: i64) -> Result<String> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;

    if timestamp_micros.rem_euclid(MICROS_PER_DAY) != 0 {
        return Err(HtapError::InvalidArgument(format!(
            "timestamp is not UTC midnight: {timestamp_micros}"
        )));
    }

    // Howard Hinnant's inverse civil-date algorithm.
    let days = timestamp_micros.div_euclid(MICROS_PER_DAY);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);

    if !(0..=9999).contains(&year) {
        return Err(HtapError::InvalidArgument(format!(
            "calendar date year out of range: {year}"
        )));
    }

    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

/// Parses a plain decimal numeral into an unscaled value at `scale`, rounding
/// discarded fractional digits half away from zero.
///
/// This backward-compatible wrapper discards the exactness result from
/// [`parse_decimal_text_with_exactness`].
pub fn parse_decimal_text(value: &str, scale: u8) -> Result<i128> {
    parse_decimal_text_with_exactness(value, scale).map(|(unscaled, _)| unscaled)
}

/// Returns whether a plain decimal numeral is exactly representable at `scale`.
///
/// A numeral is exact when every fractional digit beyond `scale` is zero.
/// The syntax accepted here matches [`parse_decimal_text_with_exactness`].
pub fn decimal_text_is_exact_at_scale(value: &str, scale: u8) -> Result<bool> {
    let parse_error = || HtapError::InvalidArgument(format!("invalid decimal text: '{value}'"));

    let value = match value.as_bytes().first() {
        Some(b'-') | Some(b'+') => &value[1..],
        _ => value,
    };
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) if !fraction.contains('.') => (whole, Some(fraction)),
        Some(_) => return Err(parse_error()),
        None => (value, None),
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(parse_error());
    }

    Ok(fraction
        .unwrap_or("")
        .as_bytes()
        .get(usize::from(scale)..)
        .is_none_or(|discarded| discarded.iter().all(|digit| *digit == b'0')))
}

/// Parses a plain decimal numeral into an unscaled value at `scale`, rounding
/// discarded fractional digits half away from zero.
///
/// Returns the unscaled value and whether the conversion was exact. A
/// conversion is inexact when any fractional digit discarded beyond `scale` is
/// non-zero, including digits that do not affect rounding.
pub fn parse_decimal_text_with_exactness(value: &str, scale: u8) -> Result<(i128, bool)> {
    let parse_error = || HtapError::InvalidArgument(format!("invalid decimal text: '{value}'"));

    let (negative, value) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) if !fraction.contains('.') => (whole, Some(fraction)),
        Some(_) => return Err(parse_error()),
        None => (value, None),
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(parse_error());
    }

    let factor = 10_i128
        .checked_pow(u32::from(scale))
        .ok_or_else(parse_error)?;
    let whole = whole.bytes().try_fold(0_i128, |number, byte| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(i128::from(byte - b'0')))
            .ok_or_else(parse_error)
    })?;
    let mut unscaled = whole.checked_mul(factor).ok_or_else(parse_error)?;
    let mut exact = true;

    if let Some(fraction) = fraction {
        let mut kept = 0_i128;
        for index in 0..usize::from(scale) {
            kept = kept
                .checked_mul(10)
                .and_then(|number| {
                    number.checked_add(i128::from(
                        fraction.as_bytes().get(index).copied().unwrap_or(b'0') - b'0',
                    ))
                })
                .ok_or_else(parse_error)?;
        }
        unscaled = unscaled.checked_add(kept).ok_or_else(parse_error)?;

        let discarded = &fraction.as_bytes()[usize::from(scale).min(fraction.len())..];
        exact = discarded.iter().all(|digit| *digit == b'0');

        if discarded.first().is_some_and(|digit| *digit >= b'5') {
            unscaled = unscaled.checked_add(1).ok_or_else(parse_error)?;
        }
    }

    let unscaled = if negative {
        unscaled.checked_neg().ok_or_else(parse_error)?
    } else {
        unscaled
    };
    Ok((unscaled, exact))
}

/// A rewritten DECIMAL predicate literal or its resolved constant result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecimalPredicateLiteral {
    /// A representable DECIMAL literal to retain with the original comparison operator.
    Value(Value),
    /// A comparison result that is independent of the column value, except that SQL NULL
    /// filtering remains the caller's responsibility.
    Constant(bool),
}

/// Rewrites a decimal comparison literal to the nearest representable DECIMAL value.
///
/// `operator` must be one of `=`, `!=`, `>`, `>=`, `<`, or `<=`. Exact literals
/// return their declared-scale representation. Non-representable literals are either
/// rewritten to a representable boundary or resolved to a boolean constant.
pub fn rewrite_decimal_predicate_literal(
    text: &str,
    precision: u8,
    scale: u8,
    operator: &str,
) -> Result<DecimalPredicateLiteral> {
    DataType::Decimal { precision, scale }.validate()?;

    let invalid = || HtapError::InvalidArgument(format!("invalid decimal text: '{text}'"));
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) if !fraction.contains('.') => (whole, Some(fraction)),
        Some(_) => return Err(invalid()),
        None => (unsigned, None),
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_some_and(|fraction| {
            fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(invalid());
    }

    let factor = 10_i128.checked_pow(u32::from(scale)).ok_or_else(invalid)?;
    let whole = whole.bytes().try_fold(0_i128, |number, byte| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(i128::from(byte - b'0')))
            .ok_or_else(invalid)
    })?;
    let kept = fraction
        .unwrap_or("")
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(usize::from(scale))
        .try_fold(0_i128, |number, byte| {
            number
                .checked_mul(10)
                .and_then(|number| number.checked_add(i128::from(byte - b'0')))
                .ok_or_else(invalid)
        })?;
    let magnitude = whole
        .checked_mul(factor)
        .and_then(|value| value.checked_add(kept));
    let magnitude = magnitude.ok_or_else(invalid)?;
    let discarded_nonzero = fraction
        .unwrap_or("")
        .as_bytes()
        .get(usize::from(scale)..)
        .is_some_and(|discarded| discarded.iter().any(|digit| *digit != b'0'));
    let exact = !discarded_nonzero;

    let unscaled = if exact {
        if negative {
            magnitude.checked_neg().ok_or_else(invalid)?
        } else {
            magnitude
        }
    } else {
        match operator {
            "=" => return Ok(DecimalPredicateLiteral::Constant(false)),
            "!=" => return Ok(DecimalPredicateLiteral::Constant(true)),
            ">" | ">=" => {
                if negative {
                    magnitude.checked_neg().ok_or_else(invalid)?
                } else {
                    magnitude.checked_add(1).ok_or_else(invalid)?
                }
            }
            "<" | "<=" => {
                if negative {
                    magnitude
                        .checked_add(1)
                        .and_then(|value| value.checked_neg())
                        .ok_or_else(invalid)?
                } else {
                    magnitude
                }
            }
            _ => {
                return Err(HtapError::InvalidArgument(format!(
                    "unsupported decimal comparison operator '{operator}'"
                )))
            }
        }
    };

    let max = 10_i128
        .checked_pow(u32::from(precision))
        .ok_or_else(invalid)?
        - 1;
    if unscaled > max {
        return Ok(DecimalPredicateLiteral::Constant(matches!(
            operator,
            "!=" | "<" | "<="
        )));
    }
    if unscaled < -max {
        return Ok(DecimalPredicateLiteral::Constant(matches!(
            operator,
            "!=" | ">" | ">="
        )));
    }

    let unscaled = i64::try_from(unscaled).map_err(|_| {
        HtapError::InvalidArgument(format!(
            "decimal overflow for DECIMAL({precision},{scale}): '{text}'"
        ))
    })?;
    check_decimal_precision(unscaled, precision, scale)?;

    Ok(DecimalPredicateLiteral::Value(Value::Decimal {
        value: unscaled,
        precision,
        scale,
    }))
}

/// Validates that an unscaled decimal value fits its declared precision.
pub fn check_decimal_precision(value: i64, precision: u8, scale: u8) -> Result<i64> {
    let limit = 10_i128.checked_pow(u32::from(precision)).ok_or_else(|| {
        HtapError::InvalidArgument(format!(
            "decimal precision {precision} is too large for value {value} at scale {scale}"
        ))
    })?;
    if i128::from(value).abs() >= limit {
        return Err(HtapError::InvalidArgument(format!(
            "decimal value {value} exceeds DECIMAL({precision},{scale}) precision"
        )));
    }
    Ok(value)
}

/// A single dynamically typed value.
///
/// # Ordering and equality
///
/// `Value` is used as (part of) an index key, and an index key **must** have a
/// total order: every pair of values has to compare as exactly one of
/// `Less`/`Equal`/`Greater`, and `Eq`, `Hash` and `Ord` must all agree.
///
/// IEEE-754 float semantics are incompatible with that requirement, because
/// `NaN != NaN` and `NaN` is unordered with respect to everything. We therefore
/// **deliberately deviate from IEEE-754**: `Float64` is compared with
/// [`f64::total_cmp`], which yields the totally ordered
/// `-NaN < -inf < .. < -0.0 < 0.0 < .. < +inf < +NaN`. As a consequence
/// `Value::Float64(f64::NAN) == Value::Float64(f64::NAN)` holds, and `-0.0` and
/// `0.0` are *not* equal to each other. `Hash` matches this by hashing
/// `f64::to_bits()`.
///
/// `PartialEq` is hand-written for exactly this reason: the derived
/// implementation would delegate to `f64`'s IEEE comparison, making `NaN` not
/// equal to itself and thereby violating the `Eq`/`Hash`/`Ord` contracts.
///
/// `Null` sorts before every non-null value. Two non-null values of different
/// types are ordered by their [`DataType`] discriminant, which keeps the
/// ordering total and deterministic even for heterogeneous input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    /// SQL NULL. Sorts before every non-null value.
    Null,
    /// Boolean.
    Bool(bool),
    /// Signed 32-bit integer.
    Int32(i32),
    /// Signed 64-bit integer.
    Int64(i64),
    /// IEEE-754 double, totally ordered via [`f64::total_cmp`].
    Float64(f64),
    /// UTF-8 string.
    String(String),
    /// Opaque byte string.
    Bytes(Vec<u8>),
    /// Microseconds since the Unix epoch.
    Timestamp(i64),
    /// Fixed-point decimal represented by an unscaled integer.
    Decimal {
        /// Unscaled integer value.
        value: i64,
        /// Total number of decimal digits.
        precision: u8,
        /// Number of digits after the decimal point.
        scale: u8,
    },
}

impl Value {
    /// Logical type of this value, or `None` for [`Value::Null`] (a NULL
    /// carries no type of its own; the column definition supplies it).
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(DataType::Bool),
            Value::Int32(_) => Some(DataType::Int32),
            Value::Int64(_) => Some(DataType::Int64),
            Value::Float64(_) => Some(DataType::Float64),
            Value::String(_) => Some(DataType::String),
            Value::Bytes(_) => Some(DataType::Bytes),
            Value::Timestamp(_) => Some(DataType::Timestamp),
            Value::Decimal {
                precision, scale, ..
            } => Some(DataType::Decimal {
                precision: *precision,
                scale: *scale,
            }),
        }
    }

    /// Whether this value is [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Sort rank: `0` for NULL, otherwise the [`DataType`] rank shifted by one
    /// so that NULL always sorts first.
    #[inline]
    fn sort_rank(&self) -> u8 {
        match self.data_type() {
            None => 0,
            Some(dt) => dt.rank() + 1,
        }
    }
}

fn format_decimal(value: i128, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }

    let negative = value < 0;
    let mut digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);

    if digits.len() <= scale {
        let leading_zeros = scale + 1 - digits.len();
        digits.insert_str(0, &"0".repeat(leading_zeros));
    }

    let decimal_point = digits.len() - scale;
    digits.insert(decimal_point, '.');

    if negative {
        digits.insert(0, '-');
    }
    digits
}

/// Compares two scaled decimal integers without scale alignment multiplication.
fn compare_decimal(a: i64, a_scale: u8, b: i64, b_scale: u8) -> Ordering {
    let sign = a.signum().cmp(&b.signum());
    if sign != Ordering::Equal || a == 0 {
        return sign;
    }

    let normalize = |value: i64, mut scale: u8| {
        let mut magnitude = value.unsigned_abs();
        while scale > 0 && magnitude.is_multiple_of(10) {
            magnitude /= 10;
            scale -= 1;
        }
        (magnitude, scale)
    };
    let digit_count = |mut magnitude: u64| {
        let mut count = 0;
        while magnitude > 0 {
            magnitude /= 10;
            count += 1;
        }
        count
    };

    let (a_magnitude, a_scale) = normalize(a, a_scale);
    let (b_magnitude, b_scale) = normalize(b, b_scale);
    let a_digits = digit_count(a_magnitude);
    let b_digits = digit_count(b_magnitude);
    let a_position = a_digits as i16 - i16::from(a_scale);
    let b_position = b_digits as i16 - i16::from(b_scale);
    let magnitude = match a_position.cmp(&b_position) {
        Ordering::Equal => {
            // Matching positions bound the necessary alignment by the stored integer width.
            // The scaled value therefore always fits in the wider operand's u64 magnitude.
            if a_digits < b_digits {
                let factor = 10_u64.pow(b_digits - a_digits);
                (a_magnitude * factor).cmp(&b_magnitude)
            } else if b_digits < a_digits {
                let factor = 10_u64.pow(a_digits - b_digits);
                a_magnitude.cmp(&(b_magnitude * factor))
            } else {
                a_magnitude.cmp(&b_magnitude)
            }
        }
        ordering => ordering,
    };

    if a < 0 {
        magnitude.reverse()
    } else {
        magnitude
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int32(v) => write!(f, "{v}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::String(v) => write!(f, "{v}"),
            Value::Bytes(v) => {
                for b in v {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
            Value::Timestamp(v) => write!(f, "{v}"),
            Value::Decimal { value, scale, .. } => {
                f.write_str(&format_decimal(i128::from(*value), *scale))
            }
        }
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int32(a), Value::Int32(b)) => a.cmp(b),
            (Value::Int64(a), Value::Int64(b)) => a.cmp(b),
            // Total order over floats, NaN included. See the type docs.
            (Value::Float64(a), Value::Float64(b)) => a.total_cmp(b),
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            (
                Value::Decimal {
                    value: a,
                    scale: a_scale,
                    ..
                },
                Value::Decimal {
                    value: b,
                    scale: b_scale,
                    ..
                },
            ) => compare_decimal(*a, *a_scale, *b, *b_scale),
            // Different variants: fall back to the discriminant order so the
            // relation stays total and deterministic.
            _ => self.sort_rank().cmp(&other.sort_rank()),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Hand-written so that `Float64(NaN) == Float64(NaN)`; deriving `PartialEq`
// would use IEEE-754 semantics and break the `Eq`/`Hash`/`Ord` contract.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.sort_rank().hash(state);
        match self {
            Value::Null => {}
            Value::Bool(v) => v.hash(state),
            Value::Int32(v) => v.hash(state),
            Value::Int64(v) => v.hash(state),
            // Hash the raw bits so equal-by-`total_cmp` floats hash equally.
            Value::Float64(v) => v.to_bits().hash(state),
            Value::String(v) => v.hash(state),
            Value::Bytes(v) => v.hash(state),
            Value::Timestamp(v) => v.hash(state),
            Value::Decimal { value, scale, .. } => {
                let mut value = *value;
                let mut scale = *scale;
                while scale > 0 && value % 10 == 0 {
                    value /= 10;
                    scale -= 1;
                }
                scale.hash(state);
                value.hash(state);
            }
        }
    }
}

/// Definition of a single column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColumnDef {
    /// Column name, unique within a [`Schema`].
    pub name: String,
    /// Logical type.
    pub data_type: DataType,
    /// Whether NULL is permitted.
    pub nullable: bool,
    /// Whether the column participates in the primary key.
    pub primary_key: bool,
}

/// An ordered list of [`ColumnDef`]s with unique names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    columns: Vec<ColumnDef>,
}

impl Schema {
    /// Build a schema, validating that it is non-empty and that all column
    /// names are unique.
    ///
    /// # Errors
    ///
    /// Returns [`HtapError::InvalidArgument`] if `columns` is empty or contains
    /// duplicate column names.
    pub fn new(columns: Vec<ColumnDef>) -> Result<Schema> {
        if columns.is_empty() {
            return Err(HtapError::InvalidArgument(
                "schema must have at least one column".into(),
            ));
        }
        let mut seen = std::collections::HashSet::with_capacity(columns.len());
        for col in &columns {
            col.data_type.validate()?;
            if !seen.insert(col.name.as_str()) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate column name: {}",
                    col.name
                )));
            }
        }
        Ok(Schema { columns })
    }

    /// All columns, in declaration order.
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// Number of columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Always `false`: a valid schema has at least one column. Present to
    /// satisfy the usual `len`/`is_empty` pairing.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Index of the column with the given name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Column at `idx`.
    pub fn column(&self, idx: usize) -> Option<&ColumnDef> {
        self.columns.get(idx)
    }

    /// Indices of the primary key columns, in declaration order.
    pub fn primary_key_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect()
    }
}

/// A tuple of [`Value`]s, positionally matching a [`Schema`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    values: Vec<Value>,
}

impl Row {
    /// Create a row from its values.
    pub fn new(values: Vec<Value>) -> Row {
        Row { values }
    }

    /// The values, in column order.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Value at `idx`.
    pub fn get(&self, idx: usize) -> Option<&Value> {
        self.values.get(idx)
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the row has no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Consume the row and return the owned values.
    pub fn into_values(self) -> Vec<Value> {
        self.values
    }
}

/// A write mutation on a single row key within a partition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mutation {
    /// Insert or update a row.
    Put {
        /// Target partition identifier.
        partition_id: u64,
        /// Primary key bytes.
        key: Vec<u8>,
        /// Row value.
        row: Row,
    },
    /// Delete a row (leaves an MVCC tombstone).
    Delete {
        /// Target partition identifier.
        partition_id: u64,
        /// Primary key bytes.
        key: Vec<u8>,
    },
}

impl Mutation {
    /// Target partition identifier.
    pub fn partition_id(&self) -> u64 {
        match self {
            Mutation::Put { partition_id, .. } | Mutation::Delete { partition_id, .. } => {
                *partition_id
            }
        }
    }

    /// Primary key bytes.
    pub fn key(&self) -> &[u8] {
        match self {
            Mutation::Put { key, .. } | Mutation::Delete { key, .. } => key.as_slice(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::collections::HashSet;

    fn hash_of(v: &Value) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    fn col(name: &str, data_type: DataType, primary_key: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            nullable: false,
            primary_key,
        }
    }

    #[test]
    fn test_calendar_date_timestamp_conversion() {
        assert_eq!(parse_date_to_timestamp_micros("1970-01-01").unwrap(), 0);
        assert_eq!(
            parse_date_to_timestamp_micros("1996-01-02").unwrap(),
            820_540_800_000_000
        );
        assert_eq!(
            timestamp_micros_to_date(820_540_800_000_000).unwrap(),
            "1996-01-02"
        );
        assert_eq!(
            timestamp_micros_to_date(parse_date_to_timestamp_micros("2000-02-29").unwrap())
                .unwrap(),
            "2000-02-29"
        );

        for invalid_date in ["1996-02-30", "1900-02-29", "1996-13-01", "1996-01-00"] {
            assert!(matches!(
                parse_date_to_timestamp_micros(invalid_date),
                Err(HtapError::InvalidArgument(_))
            ));
        }
        assert!(matches!(
            timestamp_micros_to_date(1),
            Err(HtapError::InvalidArgument(_))
        ));
    }

    #[test]
    fn test_data_type_names_and_display() {
        assert_eq!(DataType::Bool.name(), "bool");
        assert_eq!(DataType::Int32.name(), "int");
        assert_eq!(DataType::Int64.name(), "bigint");
        assert_eq!(DataType::Float64.name(), "double");
        assert_eq!(DataType::String.name(), "varchar");
        assert_eq!(DataType::Bytes.name(), "varbinary");
        assert_eq!(DataType::Timestamp.name(), "timestamp");
        assert_eq!(DataType::Timestamp.to_string(), "timestamp");
    }

    #[test]
    fn test_value_display_and_data_type() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Int32(-7).to_string(), "-7");
        assert_eq!(Value::String("hi".into()).to_string(), "hi");
        assert_eq!(Value::Bytes(vec![0x00, 0xff]).to_string(), "00ff");

        assert_eq!(Value::Null.data_type(), None);
        assert!(Value::Null.is_null());
        assert!(!Value::Int64(0).is_null());
        assert_eq!(Value::Int64(1).data_type(), Some(DataType::Int64));
        assert_eq!(Value::Timestamp(1).data_type(), Some(DataType::Timestamp));
    }

    #[test]
    fn test_null_sorts_before_every_non_null() {
        let non_null = [
            Value::Bool(false),
            Value::Int32(i32::MIN),
            Value::Int64(i64::MIN),
            Value::Float64(f64::NEG_INFINITY),
            Value::String(String::new()),
            Value::Bytes(Vec::new()),
            Value::Timestamp(i64::MIN),
        ];
        for v in &non_null {
            assert!(Value::Null < *v, "NULL should sort before {v:?}");
            assert!(*v > Value::Null);
        }
        assert_eq!(Value::Null, Value::Null);
    }

    #[test]
    fn test_value_ordering_same_type() {
        assert!(Value::Int64(-1) < Value::Int64(0));
        assert!(Value::Int32(i32::MIN) < Value::Int32(i32::MAX));
        assert!(Value::Bool(false) < Value::Bool(true));
        assert!(Value::String("a".into()) < Value::String("b".into()));
        assert!(Value::Bytes(vec![1]) < Value::Bytes(vec![1, 0]));
        assert!(Value::Timestamp(-5) < Value::Timestamp(5));
    }

    #[test]
    fn test_float_total_ordering_with_nan() {
        let mut floats = vec![
            Value::Float64(f64::NAN),
            Value::Float64(1.0),
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(0.0),
            Value::Float64(-0.0),
            Value::Float64(f64::INFINITY),
            Value::Float64(-1.0),
        ];
        floats.sort();
        assert_eq!(
            floats,
            vec![
                Value::Float64(f64::NEG_INFINITY),
                Value::Float64(-1.0),
                Value::Float64(-0.0),
                Value::Float64(0.0),
                Value::Float64(1.0),
                Value::Float64(f64::INFINITY),
                Value::Float64(f64::NAN),
            ]
        );
        // total_cmp distinguishes -0.0 from 0.0.
        assert!(Value::Float64(-0.0) < Value::Float64(0.0));
        assert_ne!(Value::Float64(-0.0), Value::Float64(0.0));
    }

    #[test]
    fn test_nan_equality_and_hash_consistency() {
        let a = Value::Float64(f64::NAN);
        let b = Value::Float64(f64::NAN);
        // Deliberate deviation from IEEE-754: NaN equals itself here.
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(hash_of(&a), hash_of(&b));

        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b);
        assert_eq!(set.len(), 1, "two NaNs must dedupe in a HashSet");

        set.insert(Value::Float64(0.0));
        set.insert(Value::Float64(0.0));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_cross_type_ordering_is_total_and_deterministic() {
        // Ordered by DataType discriminant: Bool < Int32 < Int64 < Float64 <
        // String < Bytes < Timestamp, with NULL first.
        let mut vals = vec![
            Value::Timestamp(0),
            Value::Bytes(vec![0]),
            Value::String("z".into()),
            Value::Float64(9.0),
            Value::Int64(9),
            Value::Int32(9),
            Value::Bool(true),
            Value::Null,
        ];
        vals.sort();
        assert_eq!(
            vals,
            vec![
                Value::Null,
                Value::Bool(true),
                Value::Int32(9),
                Value::Int64(9),
                Value::Float64(9.0),
                Value::String("z".into()),
                Value::Bytes(vec![0]),
                Value::Timestamp(0),
            ]
        );
        // Numeric equality across types does NOT hold; type rank decides.
        assert_ne!(Value::Int32(9), Value::Int64(9));
        assert!(Value::Int32(i32::MAX) < Value::Int64(i64::MIN));
    }

    #[test]
    fn test_decimal_display_ordering_and_hashing() {
        let a = Value::Decimal {
            value: 12_345,
            precision: 7,
            scale: 2,
        };
        let same = Value::Decimal {
            value: 12_345,
            precision: 7,
            scale: 2,
        };
        let larger = Value::Decimal {
            value: 12_346,
            precision: 7,
            scale: 2,
        };
        let negative = Value::Decimal {
            value: -5,
            precision: 4,
            scale: 2,
        };

        assert_eq!(
            DataType::Decimal {
                precision: 7,
                scale: 2
            }
            .name(),
            "decimal"
        );
        assert_eq!(a.to_string(), "123.45");
        assert_eq!(negative.to_string(), "-0.05");
        assert_eq!(a, same);
        assert!(a < larger);
        assert!(Value::Timestamp(i64::MAX) < negative);
        assert_eq!(hash_of(&a), hash_of(&same));
    }

    #[test]
    fn test_decimal_numeric_ordering_and_hashing_across_representations() {
        let one_hundredths_precision_five = Value::Decimal {
            value: 100,
            precision: 5,
            scale: 2,
        };
        let one_hundredths_precision_three = Value::Decimal {
            value: 100,
            precision: 3,
            scale: 2,
        };
        let one_tenth = Value::Decimal {
            value: 10,
            precision: 2,
            scale: 1,
        };
        let nine_ninety_nine = Value::Decimal {
            value: 999,
            precision: 3,
            scale: 2,
        };
        let negative_one_tenth = Value::Decimal {
            value: -10,
            precision: 2,
            scale: 1,
        };
        let negative_one_hundredths = Value::Decimal {
            value: -100,
            precision: 5,
            scale: 2,
        };

        // Equality and ordering compare the numeric value, not precision or scale.
        assert_eq!(
            one_hundredths_precision_five,
            one_hundredths_precision_three
        );
        assert_eq!(one_hundredths_precision_three, one_tenth);
        assert_eq!(
            one_hundredths_precision_three.cmp(&nine_ninety_nine),
            Ordering::Less
        );
        assert!(nine_ninety_nine > one_hundredths_precision_five);

        // Equivalent normalized decimal values must produce identical hashes.
        assert_eq!(
            hash_of(&one_hundredths_precision_five),
            hash_of(&one_hundredths_precision_three)
        );
        assert_eq!(
            hash_of(&one_hundredths_precision_three),
            hash_of(&one_tenth)
        );

        // The same numeric equality, ordering, and hash guarantees apply to negatives.
        assert_eq!(negative_one_tenth, negative_one_hundredths);
        assert_eq!(
            negative_one_hundredths.cmp(&one_hundredths_precision_three),
            Ordering::Less
        );
        assert!(negative_one_hundredths < one_hundredths_precision_three);
        assert_eq!(
            hash_of(&negative_one_tenth),
            hash_of(&negative_one_hundredths)
        );

        // These cannot arise from a validated DECIMAL type, but comparisons must still retain
        // an exact total numeric order without overflowing scale alignment.
        let huge_scale = Value::Decimal {
            value: 1,
            precision: 255,
            scale: 255,
        };
        let whole = Value::Decimal {
            value: 1,
            precision: 1,
            scale: 0,
        };
        let tenth = Value::Decimal {
            value: 1,
            precision: 1,
            scale: 1,
        };
        assert_eq!(huge_scale.cmp(&whole), Ordering::Less);
        assert_eq!(whole.cmp(&huge_scale), Ordering::Greater);

        // The ordering remains transitive when one operand has an extreme scale.
        assert!(huge_scale < tenth);
        assert!(tenth < whole);
        assert!(huge_scale < whole);
    }

    #[test]
    fn test_decimal_predicate_literals_beyond_declared_precision_resolve_to_constants() {
        let operators = ["=", "!=", "<", "<=", ">", ">="];
        let above = [false, true, true, true, false, false];
        let below = [false, true, false, false, true, true];

        for (operator, expected) in operators.iter().zip(above) {
            assert_eq!(
                rewrite_decimal_predicate_literal("100", 2, 0, operator).unwrap(),
                DecimalPredicateLiteral::Constant(expected),
                "operator {operator} above DECIMAL(2,0)"
            );
        }
        for (operator, expected) in operators.iter().zip(below) {
            assert_eq!(
                rewrite_decimal_predicate_literal("-100", 2, 0, operator).unwrap(),
                DecimalPredicateLiteral::Constant(expected),
                "operator {operator} below DECIMAL(2,0)"
            );
        }

        for text in ["99", "-99"] {
            let DecimalPredicateLiteral::Value(Value::Decimal {
                value,
                precision,
                scale,
            }) = rewrite_decimal_predicate_literal(text, 2, 0, "=").unwrap()
            else {
                panic!("representable boundary literal should remain a DECIMAL value");
            };
            assert_eq!(value, if text.starts_with('-') { -99 } else { 99 });
            assert_eq!(precision, 2);
            assert_eq!(scale, 0);
        }
    }

    #[test]
    fn test_schema_new_rejects_duplicates_and_empty() {
        let err = Schema::new(vec![]).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
        assert!(err.to_string().contains("at least one column"));

        let dup = Schema::new(vec![
            col("id", DataType::Int64, true),
            col("id", DataType::String, false),
        ])
        .unwrap_err();
        assert!(matches!(dup, HtapError::InvalidArgument(_)));
        assert!(dup.to_string().contains("duplicate column name: id"));
    }

    #[test]
    fn test_schema_accessors_and_primary_key_indices() {
        let schema = Schema::new(vec![
            col("tenant", DataType::String, true),
            col("payload", DataType::Bytes, false),
            col("id", DataType::Int64, true),
        ])
        .unwrap();

        assert_eq!(schema.len(), 3);
        assert!(!schema.is_empty());
        assert_eq!(schema.columns().len(), 3);
        assert_eq!(schema.column_index("id"), Some(2));
        assert_eq!(schema.column_index("missing"), None);
        assert_eq!(schema.column(1).unwrap().name, "payload");
        assert_eq!(schema.column(3), None);
        // Declaration order, not sorted by name.
        assert_eq!(schema.primary_key_indices(), vec![0, 2]);

        let no_pk = Schema::new(vec![col("a", DataType::Bool, false)]).unwrap();
        assert!(no_pk.primary_key_indices().is_empty());
    }

    #[test]
    fn test_row_accessors() {
        let row = Row::new(vec![Value::Int64(1), Value::Null]);
        assert_eq!(row.len(), 2);
        assert!(!row.is_empty());
        assert_eq!(row.get(0), Some(&Value::Int64(1)));
        assert_eq!(row.get(1), Some(&Value::Null));
        assert_eq!(row.get(2), None);
        assert_eq!(row.values(), &[Value::Int64(1), Value::Null]);
        assert_eq!(
            row.clone().into_values(),
            vec![Value::Int64(1), Value::Null]
        );
        assert!(Row::new(vec![]).is_empty());
    }

    #[test]
    fn test_serde_roundtrip() {
        let schema = Schema::new(vec![col("id", DataType::Int64, true)]).unwrap();
        let json = serde_json::to_string(&schema).unwrap();
        assert_eq!(schema, serde_json::from_str::<Schema>(&json).unwrap());

        let row = Row::new(vec![
            Value::Null,
            Value::Float64(1.5),
            Value::String("x".into()),
        ]);
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(row, serde_json::from_str::<Row>(&json).unwrap());
    }

    #[test]
    fn test_serde_json_float64_roundtrips_bit_identically() {
        let values = [
            0.1_f64,
            -0.0_f64,
            1.000_000_000_000_000_2_f64,
            1.234_567_890_123_456_7_f64,
            1e-308_f64,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::MAX,
        ];

        for value in values {
            let original = Value::Float64(value);
            let json = serde_json::to_string(&original).unwrap();
            let decoded = serde_json::from_str::<Value>(&json).unwrap();
            let Value::Float64(decoded) = decoded else {
                panic!("expected Float64 after JSON roundtrip");
            };
            assert_eq!(
                decoded.to_bits(),
                value.to_bits(),
                "JSON roundtrip changed DOUBLE bits for {value:?}: {json}"
            );
        }
    }
}
