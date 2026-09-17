//! Binary protocol codec: `COM_STMT_PREPARE_OK` and `COM_STMT_EXECUTE`/binary-resultset-row
//! encode/decode helpers.
//!
//! Pure functions only (no I/O, no [`htap_server::Session`] access): packet framing and
//! statement-registry/session wiring belong to Phase 11 plan task 4 (`prepared.rs` /
//! `server.rs`), not here. This module only decodes/encodes payload bytes.
//!
//! # Protocol facts verified against `mysql_common` (the crate underlying the `mysql`/
//! `mysql_async` client crates), version 0.37.3, since this workspace has no MySQL server
//! reference implementation to test against directly:
//!
//! - **NULL-bitmap offsets.** `mysql_common::packets::NullBitmap<T>::bitmap_len` computes
//!   `(num_columns + 7 + T::BIT_OFFSET) / 8`, with `ClientSide::BIT_OFFSET = 0` (the bitmap a
//!   client builds for `COM_STMT_EXECUTE` parameters) and `ServerSide::BIT_OFFSET = 2` (the
//!   bitmap a server builds for a binary resultset row) — `mysql_common-0.37.3/src/value/mod.rs`
//!   lines ~29-46. This matches the public MySQL protocol documentation's separately stated
//!   formulas (`(num_params+7)/8` for `COM_STMT_EXECUTE`, `(num_fields+9)/8` for a binary row)
//!   and is why [`decode_execute`] uses offset 0 and [`encode_binary_row`] uses offset 2.
//! - **`new_params_bound_flag` / per-statement type caching (amendment A2).** The `mysql_common`
//!   client-side builder (`ComStmtExecuteRequestBuilder::build`,
//!   `mysql_common-0.37.3/src/packets/mod.rs` line ~2694) always sets
//!   `StmtExecuteParamsFlags::NEW_PARAMS_BOUND` (flag = 1) and always resends every parameter's
//!   type — it never sends flag = 0. `mysql_common` does not implement a client that ever omits
//!   parameter types, so it cannot itself confirm the flag = 0 caching path; that behavior is
//!   documented in the public MySQL C API (`mysql_stmt_execute()`/`mysql_stmt_bind_param()`
//!   only resend bind metadata when `stmt->bind_result_done`/param buffer types actually
//!   changed) and the wire protocol docs, which state flag = 0 means "reuse the types from the
//!   last `COM_STMT_EXECUTE` that set flag = 1" — this workspace has no way to execute the real
//!   C client to confirm byte-for-byte, so [`decode_execute`] implements the documented behavior
//!   (a per-statement cache the *caller* owns and passes in) and returns a clean protocol error
//!   when flag = 0 arrives with no cache, rather than guessing.
//! - **`COM_STMT_SEND_LONG_DATA` and omitted parameter values.**
//!   `ComStmtExecuteRequest::serialize` (`mysql_common-0.37.3/src/packets/mod.rs` lines
//!   ~2798-2813) always writes every parameter's `(type, flags)` byte pair, but skips writing a
//!   `Value::Bytes` parameter's *value* bytes once the whole request is flagged `as_long_data`
//!   (computed request-wide, when the total encoded length would exceed `MAX_PAYLOAD_LEN`) —
//!   confirming the plan's claim that a long-data parameter's type is still transmitted but its
//!   value bytes are not. Note this client always makes that decision for *every* `Bytes`
//!   parameter in a request together (an implementation choice of this particular client, not a
//!   documented protocol requirement); the protocol itself has no per-parameter marker in the
//!   `COM_STMT_EXECUTE` payload distinguishing "value omitted because sent via
//!   `COM_STMT_SEND_LONG_DATA`" from any other case — a server can only know this from having
//!   already recorded, per statement and parameter index, that a prior
//!   `COM_STMT_SEND_LONG_DATA` packet targeted it. [`decode_execute`] therefore takes that
//!   knowledge as an explicit `long_data_pending` argument (the statement registry Phase 11 plan
//!   task 4 will build owns that state) rather than trying to infer it from the payload alone —
//!   deliberately deviating from amendment A2's suggested three-argument signature, which cannot
//!   be implemented correctly without this information; see [`decode_execute`]'s doc comment.
//! - **`MYSQL_TYPE_INT24` wire width.** `Value::deserialize_bin` (`mysql_common-0.37.3/src/
//!   value/mod.rs` line ~429) decodes `MYSQL_TYPE_INT24` with `deserialize_long`, i.e. the same
//!   4-byte little-endian form as `MYSQL_TYPE_LONG`, not a 3-byte form — "24" describes the SQL
//!   column's display width, not its wire encoding. [`decode_execute`] follows this.
//! - **`String` vs `Vec<u8>` bound parameters.** `ComStmtExecuteRequest::serialize`'s type
//!   selection (same file, lines ~2769-2793) maps `mysql_common::Value::Bytes` — the variant
//!   both a bound Rust `String` and a bound Rust `Vec<u8>` convert to on this client (see
//!   `mysql_common`'s `impl From<String> for Value` / `impl From<Vec<u8>> for Value`, both
//!   producing `Value::Bytes`) — to `MYSQL_TYPE_VAR_STRING` unconditionally. There is no
//!   `MYSQL_TYPE_BLOB`/binary marker distinguishing "this came from a Rust `String`" from "this
//!   came from a Rust `Vec<u8>`" for a *bound parameter* the way `BINARY_FLAG`/collation 63 does
//!   for a *result column*: both arrive on the wire exactly the same way, as
//!   `MYSQL_TYPE_VAR_STRING` with length-encoded bytes. [`decode_execute`] resolves this without
//!   ever discarding bytes: a `MYSQL_TYPE_VAR_STRING`/`STRING`/`VARCHAR`/`ENUM`/`SET` parameter's
//!   raw bytes decode as `Value::String` when they are valid UTF-8 (indistinguishable from a
//!   `String` parameter, exactly like real MySQL text-typed columns) and as `Value::Bytes`
//!   otherwise (a `Vec<u8>` parameter that happens not to be valid UTF-8) — never lossily,
//!   unlike an earlier version of this function. Only the genuine `*_BLOB` type codes always
//!   decode as `Value::Bytes` regardless of UTF-8 validity.

use std::io;

use htap_common::types::{ColumnDef, Row, Value};

use crate::codec::{read_fixed, read_lenenc_str, read_u16, read_u32, write_lenenc_str};
use crate::proto::*;
use crate::result_codec::mysql_type_for;

/// Per-parameter unsigned marker in a `COM_STMT_EXECUTE` type/flags byte pair (distinct from
/// [`crate::proto::UNSIGNED_FLAG`], which is a *column-definition* flag).
const PARAM_UNSIGNED_FLAG: u8 = 0x80;

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// A parameter's declared wire type: the `(type, unsigned)` pair from a `COM_STMT_EXECUTE`
/// type/flags byte pair, cached across executes that set `new_params_bound_flag = 0`
/// (Phase 11 plan amendment A2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamType {
    /// Raw `MYSQL_TYPE_*` byte.
    pub mysql_type: u8,
    /// Whether the `0x80` unsigned bit was set alongside this type.
    pub unsigned: bool,
}

/// One decoded bound parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    /// A value that maps directly onto an engine [`Value`].
    Value(Value),
    /// A `NEWDECIMAL`/`MYSQL_TYPE_DECIMAL` parameter, kept as its original length-encoded text
    /// (Phase 11 plan amendment A3) rather than parsed through `f64`, so a caller can substitute
    /// it as a numeric literal (`Value::Number` in `sqlparser`'s AST) and let the binder's own
    /// numeric-literal handling apply with no precision loss.
    DecimalText(String),
    /// This parameter's value was supplied out-of-band via `COM_STMT_SEND_LONG_DATA`: the
    /// `COM_STMT_EXECUTE` payload carried this parameter's type but (per `long_data_pending`,
    /// see [`decode_execute`]) no value bytes for it. Never produced for a parameter whose NULL
    /// bit is set (checked first).
    LongDataPlaceholder,
}

/// Result of decoding a `COM_STMT_EXECUTE` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedExecute {
    /// Statement id (`stmt_id` field).
    pub stmt_id: u32,
    /// Every parameter's wire type, in order — either freshly decoded (flag = 1) or the
    /// caller-supplied cache echoed back unchanged (flag = 0), so the caller can always update
    /// its own cache from this field regardless of which path was taken.
    pub types: Vec<ParamType>,
    /// Every parameter's decoded value, in order.
    pub values: Vec<ParamValue>,
}

/// Decodes a `COM_STMT_EXECUTE` payload (after the leading command byte has already been
/// stripped by the caller).
///
/// Layout: 4-byte `stmt_id`, 1-byte `flags` (must be [`CURSOR_TYPE_NO_CURSOR`]; any other cursor
/// type is rejected — this server has no server-side cursor support), 4-byte
/// `iteration_count` (read and ignored; MySQL clients always send 1), then, only if
/// `num_params > 0`: a NULL-bitmap at offset 0 (`(num_params + 7) / 8` bytes — see the module
/// doc comment), a 1-byte `new_params_bound_flag`, `num_params` `(type, unsigned)` byte pairs
/// when that flag is 1 (absent when it is 0), and finally each non-NULL, non-long-data
/// parameter's value bytes in order.
///
/// `num_params` is the statement's own parameter count (from the registry that owns the
/// prepared statement, not decoded from this payload — the wire format never repeats it here).
///
/// `cached_types` is the caller's last-cached `(type, unsigned)` list for this statement, if
/// any (amendment A2); used only when `new_params_bound_flag = 0`, in which case its length
/// must equal `num_params` or this returns a clean error. Passing `None` when the flag is 0
/// also returns a clean error (no silent guess at parameter types).
///
/// `long_data_pending` must have exactly `num_params` entries; `long_data_pending[i] = true`
/// means the caller's statement registry has already accumulated bytes for parameter `i` via
/// one or more prior `COM_STMT_SEND_LONG_DATA` packets, so this payload carries no value bytes
/// for it (see the module doc comment on why this cannot be inferred from the payload alone).
/// Ignored for a parameter whose NULL bit is set.
///
/// # Errors
///
/// Returns a clean [`io::Error`] (`InvalidData` for a malformed/unsupported payload,
/// `UnexpectedEof` for a truncated one) and never panics: an unsupported cursor flag, a
/// `new_params_bound_flag` other than 0 or 1, flag = 0 with no or mismatched cache, an
/// unsupported/unknown parameter type (including `MYSQL_TYPE_TIME`, named in the error), an
/// unsigned `BIGINT` parameter greater than `i64::MAX`, invalid `NEWDECIMAL`/`DECIMAL` text, a
/// `long_data_pending` slice of the wrong length, or trailing bytes after the last parameter.
pub fn decode_execute(
    payload: &[u8],
    num_params: u16,
    cached_types: Option<&[ParamType]>,
    long_data_pending: &[bool],
) -> io::Result<DecodedExecute> {
    let num_params = num_params as usize;
    if long_data_pending.len() != num_params {
        return Err(bad(&format!(
            "long_data_pending has {} entries, expected {num_params}",
            long_data_pending.len()
        )));
    }

    let mut pos = 0;
    let stmt_id = read_u32(payload, &mut pos)?;
    let flags = read_fixed(payload, &mut pos, 1)?[0];
    if flags != CURSOR_TYPE_NO_CURSOR {
        return Err(bad(&format!(
            "unsupported COM_STMT_EXECUTE cursor flags 0x{flags:02x}; only CURSOR_TYPE_NO_CURSOR \
             is supported"
        )));
    }
    let _iteration_count = read_u32(payload, &mut pos)?;

    let mut null_bits = vec![false; num_params];
    let mut types: Vec<ParamType> = Vec::new();

    if num_params > 0 {
        let bitmap_len = num_params.div_ceil(8);
        let bitmap = read_fixed(payload, &mut pos, bitmap_len)?;
        for (i, null_bit) in null_bits.iter_mut().enumerate() {
            *null_bit = bitmap[i / 8] & (1 << (i % 8)) != 0;
        }

        let new_params_bound = read_fixed(payload, &mut pos, 1)?[0];
        types = match new_params_bound {
            1 => {
                let mut t = Vec::with_capacity(num_params);
                for _ in 0..num_params {
                    let pair = read_fixed(payload, &mut pos, 2)?;
                    t.push(ParamType {
                        mysql_type: pair[0],
                        unsigned: pair[1] & PARAM_UNSIGNED_FLAG != 0,
                    });
                }
                t
            }
            0 => match cached_types {
                Some(cached) if cached.len() == num_params => cached.to_vec(),
                Some(cached) => {
                    return Err(bad(&format!(
                        "new_params_bound_flag=0 but the cached parameter type list has {} \
                         entries, expected {num_params}",
                        cached.len()
                    )))
                }
                None => {
                    return Err(bad(
                        "new_params_bound_flag=0 but no parameter types are cached for this \
                         statement (the client must re-send types with new_params_bound_flag=1)",
                    ))
                }
            },
            other => {
                return Err(bad(&format!(
                    "invalid new_params_bound_flag value {other}; expected 0 or 1"
                )))
            }
        };
    }

    let mut values = Vec::with_capacity(num_params);
    for i in 0..num_params {
        if null_bits[i] {
            values.push(ParamValue::Value(Value::Null));
            continue;
        }
        if long_data_pending[i] {
            values.push(ParamValue::LongDataPlaceholder);
            continue;
        }
        values.push(decode_param_value(payload, &mut pos, types[i])?);
    }

    if pos != payload.len() {
        return Err(bad(
            "trailing bytes after the last COM_STMT_EXECUTE parameter value",
        ));
    }

    Ok(DecodedExecute {
        stmt_id,
        types,
        values,
    })
}

fn decode_param_value(payload: &[u8], pos: &mut usize, pt: ParamType) -> io::Result<ParamValue> {
    match pt.mysql_type {
        MYSQL_TYPE_NULL => Ok(ParamValue::Value(Value::Null)),
        MYSQL_TYPE_TINY => {
            let b = read_fixed(payload, pos, 1)?[0];
            let v = if pt.unsigned {
                b as i32
            } else {
                b as i8 as i32
            };
            Ok(ParamValue::Value(Value::Int32(v)))
        }
        MYSQL_TYPE_SHORT | MYSQL_TYPE_YEAR => {
            let raw = read_u16(payload, pos)?;
            let v = if pt.unsigned {
                raw as i32
            } else {
                raw as i16 as i32
            };
            Ok(ParamValue::Value(Value::Int32(v)))
        }
        MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG => {
            // Both decode as a plain 4-byte little-endian integer on the wire (see the module
            // doc comment on `MYSQL_TYPE_INT24`).
            let raw = read_u32(payload, pos)?;
            let v: i64 = if pt.unsigned {
                raw as i64
            } else {
                raw as i32 as i64
            };
            Ok(ParamValue::Value(Value::Int64(v)))
        }
        MYSQL_TYPE_LONGLONG => {
            let b = read_fixed(payload, pos, 8)?;
            let mut arr = [0u8; 8];
            arr.copy_from_slice(b);
            let bits = u64::from_le_bytes(arr);
            if pt.unsigned {
                if bits > i64::MAX as u64 {
                    return Err(bad(&format!(
                        "unsigned BIGINT parameter {bits} exceeds i64::MAX; unsigned 64-bit \
                         parameters above i64::MAX are not supported"
                    )));
                }
                Ok(ParamValue::Value(Value::Int64(bits as i64)))
            } else {
                Ok(ParamValue::Value(Value::Int64(bits as i64)))
            }
        }
        MYSQL_TYPE_FLOAT => {
            let b = read_fixed(payload, pos, 4)?;
            let mut arr = [0u8; 4];
            arr.copy_from_slice(b);
            Ok(ParamValue::Value(Value::Float64(
                f32::from_le_bytes(arr) as f64
            )))
        }
        MYSQL_TYPE_DOUBLE => {
            let b = read_fixed(payload, pos, 8)?;
            let mut arr = [0u8; 8];
            arr.copy_from_slice(b);
            Ok(ParamValue::Value(Value::Float64(f64::from_le_bytes(arr))))
        }
        MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_DECIMAL => {
            let bytes = read_lenenc_str(payload, pos)?;
            let text = std::str::from_utf8(bytes)
                .map_err(|_| bad("invalid UTF-8 in DECIMAL parameter text"))?
                .to_string();
            validate_decimal_text(&text)?;
            Ok(ParamValue::DecimalText(text))
        }
        MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_ENUM
        | MYSQL_TYPE_SET => {
            let bytes = read_lenenc_str(payload, pos)?;
            // A bound Rust `String` and a bound Rust `Vec<u8>` both arrive here as the same wire
            // type (see the module doc comment): keep the raw bytes and only decide `String` vs
            // `Bytes` from whether they are valid UTF-8, rather than lossily replacing invalid
            // bytes as an earlier version of this function did. A `Vec<u8>` parameter that
            // happens to be valid UTF-8 is indistinguishable from a `String` parameter and is
            // decoded as `Value::String`, exactly like real MySQL text-typed columns; this is the
            // same client-driven ambiguity the module doc comment describes, just resolved
            // without ever discarding bytes.
            Ok(ParamValue::Value(match String::from_utf8(bytes.to_vec()) {
                Ok(s) => Value::String(s),
                Err(e) => Value::Bytes(e.into_bytes()),
            }))
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            let bytes = read_lenenc_str(payload, pos)?;
            Ok(ParamValue::Value(Value::Bytes(bytes.to_vec())))
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
            let micros = decode_binary_datetime(payload, pos)?;
            Ok(ParamValue::Value(Value::Timestamp(micros)))
        }
        MYSQL_TYPE_TIME => Err(bad(
            "MYSQL_TYPE_TIME parameters are not supported (no engine TIME representation)",
        )),
        other => Err(bad(&format!(
            "unsupported COM_STMT_EXECUTE parameter type 0x{other:02x}"
        ))),
    }
}

/// Validates a `NEWDECIMAL`/`DECIMAL` parameter's text form: an optional leading `-`, at least
/// one digit, and at most one `.`. Rejects anything else (amendment A3: "reject non-numeric
/// text").
fn validate_decimal_text(text: &str) -> io::Result<()> {
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let mut saw_digit = false;
    let mut saw_dot = false;
    for c in unsigned.chars() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else if c == '.' && !saw_dot {
            saw_dot = true;
        } else {
            return Err(bad(&format!("invalid DECIMAL parameter text: {text:?}")));
        }
    }
    if !saw_digit {
        return Err(bad(&format!("invalid DECIMAL parameter text: {text:?}")));
    }
    Ok(())
}

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (Howard Hinnant's
/// `civil_from_days`; duplicated from `result_codec`'s private helper of the same name and
/// algorithm, since that module is off-limits to edit beyond reading — see this crate's task
/// notes).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since 1970-01-01, proleptic Gregorian.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whether `year` is a leap year, proleptic Gregorian (matches [`civil_from_days`]/
/// [`days_from_civil`]'s calendar).
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in `month` of `year` (1-indexed month; proleptic Gregorian), or `0` for an
/// out-of-range month (the caller is expected to have already rejected that).
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Decodes a binary datetime value (the 0/4/7/11-byte length-prefixed form used for
/// `MYSQL_TYPE_DATE`/`MYSQL_TYPE_DATETIME`/`MYSQL_TYPE_TIMESTAMP`) into microseconds since the
/// Unix epoch.
///
/// Every field is range-checked (finding 6 of the Phase 11 fix pass): `year` must be `0..=9999`
/// (mirroring [`encode_binary_datetime`]'s own range), `hour` `0..=23`, `minute`/`second`
/// `0..=59`, and `micros` `0..=999_999`. `month`/`day` must form a real calendar date for `year`
/// (`1..=12`, and `1..=`[`days_in_month`]`(year, month)`) with exactly one sentinel exception:
/// `month == 0 && day == 0` (regardless of which byte-length form carried it, including the
/// genuine 0-byte form, which always implies this) is MySQL's "zero date" `0000-00-00`, which has
/// no non-arbitrary representation as a Unix-epoch offset; this maps it to `0000-01-01` at the
/// given (still range-checked) time-of-day rather than panicking or silently wrapping through
/// invalid calendar arithmetic. A *partial* zero date (exactly one of `month`/`day` is 0) is not
/// this sentinel and is rejected like any other invalid calendar date. A genuine bound parameter
/// is never expected to carry a zero date in practice.
fn decode_binary_datetime(payload: &[u8], pos: &mut usize) -> io::Result<i64> {
    let len = read_fixed(payload, pos, 1)?[0];
    let (mut year, mut month, mut day, mut hour, mut minute, mut second, mut micros) =
        (0u16, 0u8, 0u8, 0u8, 0u8, 0u8, 0u32);
    match len {
        0 => {}
        4 => {
            year = read_u16(payload, pos)?;
            let b = read_fixed(payload, pos, 2)?;
            month = b[0];
            day = b[1];
        }
        7 => {
            year = read_u16(payload, pos)?;
            let b = read_fixed(payload, pos, 5)?;
            month = b[0];
            day = b[1];
            hour = b[2];
            minute = b[3];
            second = b[4];
        }
        11 => {
            year = read_u16(payload, pos)?;
            let b = read_fixed(payload, pos, 5)?;
            month = b[0];
            day = b[1];
            hour = b[2];
            minute = b[3];
            second = b[4];
            micros = read_u32(payload, pos)?;
        }
        other => {
            return Err(bad(&format!(
                "invalid binary datetime length {other}; expected 0, 4, 7, or 11"
            )))
        }
    }
    if year > 9999 {
        return Err(bad(&format!(
            "invalid binary datetime year {year}; expected 0-9999"
        )));
    }
    if month == 0 && day == 0 {
        // The zero-date sentinel; `month`/`day` are clamped to 1 below.
    } else {
        if !(1..=12).contains(&month) {
            return Err(bad(&format!(
                "invalid binary datetime month {month}; expected 1-12 (or 0 together with day 0 \
                 for the zero-date sentinel)"
            )));
        }
        let max_day = days_in_month(year as i64, month as u32);
        if day == 0 || day as u32 > max_day {
            return Err(bad(&format!(
                "invalid binary datetime day {day} for {year:04}-{month:02}; expected 1-{max_day}"
            )));
        }
    }
    if hour > 23 {
        return Err(bad(&format!(
            "invalid binary datetime hour {hour}; expected 0-23"
        )));
    }
    if minute > 59 {
        return Err(bad(&format!(
            "invalid binary datetime minute {minute}; expected 0-59"
        )));
    }
    if second > 59 {
        return Err(bad(&format!(
            "invalid binary datetime second {second}; expected 0-59"
        )));
    }
    if micros >= 1_000_000 {
        return Err(bad(&format!(
            "invalid binary datetime microseconds {micros}; expected 0-999999"
        )));
    }
    let days = days_from_civil(year as i64, (month as u32).max(1), (day as u32).max(1));
    let secs = days
        .checked_mul(SECS_PER_DAY)
        .and_then(|v| v.checked_add(hour as i64 * 3600 + minute as i64 * 60 + second as i64))
        .ok_or_else(|| bad("binary datetime parameter out of range"))?;
    secs.checked_mul(MICROS_PER_SEC)
        .and_then(|v| v.checked_add(micros as i64))
        .ok_or_else(|| bad("binary datetime parameter out of range"))
}

/// Inverse of the year/month/day/hour/minute/second/microsecond decomposition used by
/// [`decode_binary_datetime`] and [`encode_binary_datetime`].
fn ymdhmsu_from_micros(micros: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = micros.div_euclid(MICROS_PER_SEC);
    let frac = micros.rem_euclid(MICROS_PER_SEC) as u32;
    let days = secs.div_euclid(SECS_PER_DAY);
    let sod = secs.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (sod / 3600) as u32,
        ((sod % 3600) / 60) as u32,
        (sod % 60) as u32,
        frac,
    )
}

/// Encodes microseconds since the Unix epoch as a binary datetime value, choosing the shortest
/// of the 4/7/11-byte forms that round-trips exactly (never the 0-byte "zero date" form, which
/// this function never has a reason to produce: every micros value decomposes to a real
/// calendar date).
///
/// # Errors
///
/// The wire format's year field is a 2-byte unsigned integer that this server only ever
/// populates with a real calendar year `0..=9999` (finding 6 of the Phase 11 fix pass): before
/// this fix, a `Value::Timestamp` whose year fell outside `u16`'s range (let alone outside the
/// protocol's actual `0..=9999` year range) was silently truncated by an `as u16` cast, producing
/// a wrong-but-valid-looking wire value instead of an error. Truncation is no longer possible:
/// any micros value that decomposes to a year outside `0..=9999` is now a clean [`io::Error`].
fn encode_binary_datetime(micros: i64) -> io::Result<Vec<u8>> {
    let (y, m, d, hh, mm, ss, frac) = ymdhmsu_from_micros(micros);
    if !(0..=9999).contains(&y) {
        return Err(bad(&format!(
            "DATETIME value out of range: year {y} (from {micros} microseconds since the Unix \
             epoch) is outside 0-9999"
        )));
    }
    let mut buf = Vec::with_capacity(12);
    if frac != 0 {
        buf.push(11);
        buf.extend_from_slice(&(y as u16).to_le_bytes());
        buf.push(m as u8);
        buf.push(d as u8);
        buf.push(hh as u8);
        buf.push(mm as u8);
        buf.push(ss as u8);
        buf.extend_from_slice(&frac.to_le_bytes());
    } else if hh != 0 || mm != 0 || ss != 0 {
        buf.push(7);
        buf.extend_from_slice(&(y as u16).to_le_bytes());
        buf.push(m as u8);
        buf.push(d as u8);
        buf.push(hh as u8);
        buf.push(mm as u8);
        buf.push(ss as u8);
    } else {
        buf.push(4);
        buf.extend_from_slice(&(y as u16).to_le_bytes());
        buf.push(m as u8);
        buf.push(d as u8);
    }
    Ok(buf)
}

/// Fixed-size body of a `COM_STMT_PREPARE_OK` packet (the first packet of the response; the
/// following parameter/column `ColumnDefinition41` packets and separating `EOF`s are framed
/// separately by the caller using `result_codec::build_column_def41` and
/// `result_codec::build_resultset_terminator`).
///
/// Layout: 1-byte status (`0x00`), 4-byte `statement_id`, 2-byte `num_columns`, 2-byte
/// `num_params`, 1-byte reserved (`0x00`), 2-byte `warning_count`.
pub fn encode_stmt_prepare_ok(
    stmt_id: u32,
    num_columns: u16,
    num_params: u16,
    warning_count: u16,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12);
    buf.push(OK_HEADER);
    buf.extend_from_slice(&stmt_id.to_le_bytes());
    buf.extend_from_slice(&num_columns.to_le_bytes());
    buf.extend_from_slice(&num_params.to_le_bytes());
    buf.push(0);
    buf.extend_from_slice(&warning_count.to_le_bytes());
    buf
}

/// Decoded `COM_STMT_PREPARE_OK` fixed header (client side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StmtPrepareOk {
    /// Statement id assigned by the server.
    pub stmt_id: u32,
    /// Number of result columns (0 if the statement produces no result set, or its shape could
    /// not be statically inferred).
    pub num_columns: u16,
    /// Number of `?` placeholders.
    pub num_params: u16,
    /// Warning count.
    pub warning_count: u16,
}

/// Decodes a `COM_STMT_PREPARE_OK` fixed header (client side). Inverse of
/// [`encode_stmt_prepare_ok`].
pub fn decode_stmt_prepare_ok(payload: &[u8]) -> io::Result<StmtPrepareOk> {
    let mut pos = 0;
    let header = read_fixed(payload, &mut pos, 1)?[0];
    if header != OK_HEADER {
        return Err(bad("not a COM_STMT_PREPARE_OK packet"));
    }
    let stmt_id = read_u32(payload, &mut pos)?;
    let num_columns = read_u16(payload, &mut pos)?;
    let num_params = read_u16(payload, &mut pos)?;
    let _reserved = read_fixed(payload, &mut pos, 1)?;
    let warning_count = read_u16(payload, &mut pos)?;
    Ok(StmtPrepareOk {
        stmt_id,
        num_columns,
        num_params,
        warning_count,
    })
}

/// Encodes one row in the `COM_STMT_EXECUTE` binary resultset row format: a `0x00` header byte,
/// a NULL-bitmap at offset 2 (`(num_columns + 9) / 8` bytes — see the module doc comment), then
/// each non-NULL value's binary encoding, in column order.
///
/// Each value is encoded according to its column's wire type
/// (`result_codec::mysql_type_for(col.data_type)`) — exactly the type code already sent for
/// that column in the preceding `ColumnDefinition41` packet
/// (`result_codec::build_column_def41` calls the same function) — so a client that decoded the
/// column definitions decodes this row consistently. `row.values()[i]`'s variant is expected to
/// already match `columns[i].data_type` (the same invariant `result_codec::encode_text_row`
/// relies on); this is checked with `debug_assert_eq!` against the wire type code derived from
/// `columns[i]`, which never affects a release build's behavior.
///
/// # Errors
///
/// Returns a clean [`io::Error`] (never a panic) if a `Value::Timestamp` cannot be represented in
/// the wire format's `0..=9999` year range; see [`encode_binary_datetime`] (finding 6 of the
/// Phase 11 fix pass).
///
/// # Panics
///
/// Panics (debug builds only, via `debug_assert_eq!`) if a value's variant does not match its
/// column's declared type; never panics in a release build.
pub fn encode_binary_row(row: &Row, columns: &[ColumnDef]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(row.len() * 8 + 8);
    buf.push(0x00);
    let bitmap_len = (columns.len() + 9) / 8;
    let mut bitmap = vec![0u8; bitmap_len];
    for (i, value) in row.values().iter().enumerate() {
        if matches!(value, Value::Null) {
            let bit_pos = i + 2;
            bitmap[bit_pos / 8] |= 1 << (bit_pos % 8);
        }
    }
    buf.extend_from_slice(&bitmap);
    for (value, col) in row.values().iter().zip(columns) {
        let (type_code, ..) = mysql_type_for(col.data_type);
        match value {
            Value::Null => {}
            Value::Bool(b) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_TINY);
                buf.push(u8::from(*b));
            }
            Value::Int32(v) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_LONG);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Int64(v) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_LONGLONG);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::Float64(v) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_DOUBLE);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            Value::String(s) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_VAR_STRING);
                write_lenenc_str(&mut buf, s.as_bytes());
            }
            Value::Bytes(b) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_BLOB);
                write_lenenc_str(&mut buf, b);
            }
            Value::Timestamp(micros) => {
                debug_assert_eq!(type_code, MYSQL_TYPE_DATETIME);
                buf.extend_from_slice(&encode_binary_datetime(*micros)?);
            }
        }
    }
    Ok(buf)
}

/// Decodes one row from the `COM_STMT_EXECUTE` binary resultset row format (client side).
/// Inverse of [`encode_binary_row`]: reads the `0x00` header, the offset-2 NULL bitmap, and each
/// non-NULL value according to its column's wire type (`result_codec::mysql_type_for`), exactly
/// mirroring the type matrix [`encode_binary_row`] itself produces (`TINY`/`LONG`/`LONGLONG`/
/// `DOUBLE`/`VAR_STRING`/`BLOB`/`DATETIME`; no other type is ever sent by this server).
///
/// # Errors
///
/// Returns a clean [`io::Error`] (`InvalidData`/`UnexpectedEof`, never a panic) for a header byte
/// other than `0x00`, a column wire type outside the matrix above, invalid UTF-8 in a
/// `VAR_STRING` value, or trailing bytes after the last column's value.
pub fn decode_binary_row(payload: &[u8], columns: &[ColumnDef]) -> io::Result<Row> {
    let mut pos = 0;
    let header = read_fixed(payload, &mut pos, 1)?[0];
    if header != 0x00 {
        return Err(bad(&format!(
            "invalid binary resultset row header 0x{header:02x}; expected 0x00"
        )));
    }
    let bitmap_len = (columns.len() + 9) / 8;
    let bitmap = read_fixed(payload, &mut pos, bitmap_len)?.to_vec();
    let mut values = Vec::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        let bit_pos = i + 2;
        if bitmap[bit_pos / 8] & (1 << (bit_pos % 8)) != 0 {
            values.push(Value::Null);
            continue;
        }
        let (type_code, ..) = mysql_type_for(col.data_type);
        let value = match type_code {
            MYSQL_TYPE_TINY => Value::Bool(read_fixed(payload, &mut pos, 1)?[0] != 0),
            MYSQL_TYPE_LONG => {
                let b = read_fixed(payload, &mut pos, 4)?;
                let mut arr = [0u8; 4];
                arr.copy_from_slice(b);
                Value::Int32(i32::from_le_bytes(arr))
            }
            MYSQL_TYPE_LONGLONG => {
                let b = read_fixed(payload, &mut pos, 8)?;
                let mut arr = [0u8; 8];
                arr.copy_from_slice(b);
                Value::Int64(i64::from_le_bytes(arr))
            }
            MYSQL_TYPE_DOUBLE => {
                let b = read_fixed(payload, &mut pos, 8)?;
                let mut arr = [0u8; 8];
                arr.copy_from_slice(b);
                Value::Float64(f64::from_le_bytes(arr))
            }
            MYSQL_TYPE_VAR_STRING => {
                let bytes = read_lenenc_str(payload, &mut pos)?;
                Value::String(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|_| bad("invalid UTF-8 in VAR_STRING column value"))?,
                )
            }
            MYSQL_TYPE_BLOB => Value::Bytes(read_lenenc_str(payload, &mut pos)?.to_vec()),
            MYSQL_TYPE_DATETIME => Value::Timestamp(decode_binary_datetime(payload, &mut pos)?),
            other => {
                return Err(bad(&format!(
                    "unsupported binary resultset column type 0x{other:02x}"
                )))
            }
        };
        values.push(value);
    }
    if pos != payload.len() {
        return Err(bad(
            "trailing bytes after the last binary resultset row value",
        ));
    }
    Ok(Row::new(values))
}

/// Encodes a `COM_STMT_EXECUTE` request (client side): 4-byte `stmt_id`, `CURSOR_TYPE_NO_CURSOR`
/// flags, `iteration_count = 1`, then (when `params` is non-empty) a NULL-bitmap at offset 0,
/// `new_params_bound_flag = 1`, each parameter's `(type, unsigned)` byte pair, and finally each
/// non-NULL parameter's value bytes — the inverse of [`decode_execute`] with `new_params_bound = 1`
/// always set (this client never uses the flag = 0 caching path; see the module doc comment on why
/// no real client the docs could confirm ever does either).
///
/// Type mapping (Phase 11 plan task 5): `Value::Bool` → `TINY`, `Value::Int32` → `LONG`,
/// `Value::Int64` → `LONGLONG`, `Value::Float64` → `DOUBLE`, `Value::String` → `VAR_STRING`,
/// `Value::Bytes` → `BLOB`, `Value::Timestamp` → `DATETIME` (encoded with
/// [`encode_binary_datetime`], which [`decode_execute`]'s `MYSQL_TYPE_DATE`/`DATETIME`/
/// `TIMESTAMP` arm decodes back to the exact same microsecond value — chosen over `LONGLONG`
/// specifically because it round-trips through `decode_execute` as a `Value::Timestamp`, not a
/// `Value::Int64` a caller would then have to know to reinterpret), `Value::Null` → `NULL` (type
/// byte still present, per protocol, with the NULL bit set and no value bytes).
///
/// # Errors
///
/// Returns a clean [`io::Error`] (never a panic, and never a truncated wire value) if a
/// `Value::Timestamp` parameter cannot be represented in the wire format's `0..=9999` year range
/// (finding 6 of the Phase 11 fix pass; see [`encode_binary_datetime`]).
pub fn encode_execute_request(stmt_id: u32, params: &[Value]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(16 + params.len() * 8);
    buf.extend_from_slice(&stmt_id.to_le_bytes());
    buf.push(CURSOR_TYPE_NO_CURSOR);
    buf.extend_from_slice(&1u32.to_le_bytes()); // iteration_count

    if !params.is_empty() {
        let bitmap_len = params.len().div_ceil(8);
        let mut bitmap = vec![0u8; bitmap_len];
        for (i, v) in params.iter().enumerate() {
            if matches!(v, Value::Null) {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        buf.extend_from_slice(&bitmap);
        buf.push(1); // new_params_bound_flag

        for v in params {
            let type_code = match v {
                Value::Null => MYSQL_TYPE_NULL,
                Value::Bool(_) => MYSQL_TYPE_TINY,
                Value::Int32(_) => MYSQL_TYPE_LONG,
                Value::Int64(_) => MYSQL_TYPE_LONGLONG,
                Value::Float64(_) => MYSQL_TYPE_DOUBLE,
                Value::String(_) => MYSQL_TYPE_VAR_STRING,
                Value::Bytes(_) => MYSQL_TYPE_BLOB,
                Value::Timestamp(_) => MYSQL_TYPE_DATETIME,
            };
            buf.push(type_code);
            buf.push(0); // unsigned flag: never set (this client only ever sends signed values)
        }

        for v in params {
            match v {
                Value::Null => {}
                Value::Bool(b) => buf.push(u8::from(*b)),
                Value::Int32(i) => buf.extend_from_slice(&i.to_le_bytes()),
                Value::Int64(i) => buf.extend_from_slice(&i.to_le_bytes()),
                Value::Float64(f) => buf.extend_from_slice(&f.to_le_bytes()),
                Value::String(s) => write_lenenc_str(&mut buf, s.as_bytes()),
                Value::Bytes(b) => write_lenenc_str(&mut buf, b),
                Value::Timestamp(micros) => {
                    buf.extend_from_slice(&encode_binary_datetime(*micros)?)
                }
            }
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::types::DataType;

    fn col(name: &str, dt: DataType, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: dt,
            nullable,
            primary_key: false,
        }
    }

    // -------------------------------------------------------------------------------------
    // decode_execute
    // -------------------------------------------------------------------------------------

    /// Builds a `COM_STMT_EXECUTE` payload with `new_params_bound_flag = 1` from
    /// `(type, unsigned, value_bytes)` triples, where `value_bytes` is `None` for a NULL
    /// parameter.
    fn build_execute_payload(stmt_id: u32, params: &[(u8, bool, Option<Vec<u8>>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&stmt_id.to_le_bytes());
        buf.push(CURSOR_TYPE_NO_CURSOR);
        buf.extend_from_slice(&1u32.to_le_bytes()); // iteration_count
        if !params.is_empty() {
            let bitmap_len = params.len().div_ceil(8);
            let mut bitmap = vec![0u8; bitmap_len];
            for (i, (_, _, v)) in params.iter().enumerate() {
                if v.is_none() {
                    bitmap[i / 8] |= 1 << (i % 8);
                }
            }
            buf.extend_from_slice(&bitmap);
            buf.push(1); // new_params_bound_flag
            for (t, unsigned, _) in params {
                buf.push(*t);
                buf.push(if *unsigned { PARAM_UNSIGNED_FLAG } else { 0 });
            }
            for (_, _, v) in params {
                if let Some(bytes) = v {
                    buf.extend_from_slice(bytes);
                }
            }
        }
        buf
    }

    fn lenenc_bytes(s: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_lenenc_str(&mut buf, s);
        buf
    }

    #[test]
    fn decode_execute_full_type_matrix() {
        let params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![
            (MYSQL_TYPE_NULL, false, None),
            (MYSQL_TYPE_TINY, false, Some(vec![0xff])), // -1
            (MYSQL_TYPE_TINY, true, Some(vec![0xff])),  // 255
            (
                MYSQL_TYPE_SHORT,
                false,
                Some((-1i16).to_le_bytes().to_vec()),
            ),
            (
                MYSQL_TYPE_SHORT,
                true,
                Some(65535u16.to_le_bytes().to_vec()),
            ),
            (MYSQL_TYPE_YEAR, false, Some(2024u16.to_le_bytes().to_vec())),
            (
                MYSQL_TYPE_INT24,
                false,
                Some((-1i32).to_le_bytes().to_vec()),
            ),
            (
                MYSQL_TYPE_LONG,
                true,
                Some(4_000_000_000u32.to_le_bytes().to_vec()),
            ),
            (
                MYSQL_TYPE_LONGLONG,
                false,
                Some((-42i64).to_le_bytes().to_vec()),
            ),
            (
                MYSQL_TYPE_LONGLONG,
                true,
                Some((i64::MAX as u64).to_le_bytes().to_vec()),
            ),
            (MYSQL_TYPE_FLOAT, false, Some(1.5f32.to_le_bytes().to_vec())),
            (
                MYSQL_TYPE_DOUBLE,
                false,
                Some(2.5f64.to_le_bytes().to_vec()),
            ),
            (MYSQL_TYPE_NEWDECIMAL, false, Some(lenenc_bytes(b"-123.45"))),
            (MYSQL_TYPE_DECIMAL, false, Some(lenenc_bytes(b"7"))),
            (MYSQL_TYPE_VAR_STRING, false, Some(lenenc_bytes(b"hello"))),
            (MYSQL_TYPE_VARCHAR, false, Some(lenenc_bytes(b"world"))),
            (MYSQL_TYPE_STRING, false, Some(lenenc_bytes(b"str"))),
            (MYSQL_TYPE_ENUM, false, Some(lenenc_bytes(b"enum_val"))),
            (MYSQL_TYPE_SET, false, Some(lenenc_bytes(b"a,b"))),
            (
                MYSQL_TYPE_BLOB,
                false,
                Some(lenenc_bytes(&[0x00, 0xff, 0x10])),
            ),
            (MYSQL_TYPE_TINY_BLOB, false, Some(lenenc_bytes(b"tinyblob"))),
            (
                MYSQL_TYPE_MEDIUM_BLOB,
                false,
                Some(lenenc_bytes(b"mediumblob")),
            ),
            (MYSQL_TYPE_LONG_BLOB, false, Some(lenenc_bytes(b"longblob"))),
            (MYSQL_TYPE_DATE, false, Some(vec![0])), // zero date ("0000-00-00")
            (
                MYSQL_TYPE_DATETIME,
                false,
                Some({
                    let mut b = vec![7u8];
                    b.extend_from_slice(&2023u16.to_le_bytes());
                    b.extend_from_slice(&[11, 14, 22, 13, 20]);
                    b
                }),
            ),
            (
                MYSQL_TYPE_TIMESTAMP,
                false,
                Some({
                    let mut b = vec![11u8];
                    b.extend_from_slice(&2023u16.to_le_bytes());
                    b.extend_from_slice(&[11, 14, 22, 13, 20]);
                    b.extend_from_slice(&123_456u32.to_le_bytes());
                    b
                }),
            ),
        ];
        let long_data_pending = vec![false; params.len()];
        let payload = build_execute_payload(7, &params);
        let decoded =
            decode_execute(&payload, params.len() as u16, None, &long_data_pending).unwrap();
        assert_eq!(decoded.stmt_id, 7);
        assert_eq!(decoded.types.len(), params.len());

        let expected = vec![
            ParamValue::Value(Value::Null),
            ParamValue::Value(Value::Int32(-1)),
            ParamValue::Value(Value::Int32(255)),
            ParamValue::Value(Value::Int32(-1)),
            ParamValue::Value(Value::Int32(65535)),
            ParamValue::Value(Value::Int32(2024)),
            ParamValue::Value(Value::Int64(-1)),
            ParamValue::Value(Value::Int64(4_000_000_000)),
            ParamValue::Value(Value::Int64(-42)),
            ParamValue::Value(Value::Int64(i64::MAX)),
            ParamValue::Value(Value::Float64(1.5f32 as f64)),
            ParamValue::Value(Value::Float64(2.5)),
            ParamValue::DecimalText("-123.45".into()),
            ParamValue::DecimalText("7".into()),
            ParamValue::Value(Value::String("hello".into())),
            ParamValue::Value(Value::String("world".into())),
            ParamValue::Value(Value::String("str".into())),
            ParamValue::Value(Value::String("enum_val".into())),
            ParamValue::Value(Value::String("a,b".into())),
            ParamValue::Value(Value::Bytes(vec![0x00, 0xff, 0x10])),
            ParamValue::Value(Value::Bytes(b"tinyblob".to_vec())),
            ParamValue::Value(Value::Bytes(b"mediumblob".to_vec())),
            ParamValue::Value(Value::Bytes(b"longblob".to_vec())),
            // The zero-date form ("0000-00-00") clamps month/day to 1 (see
            // `decode_binary_datetime`'s doc comment), decoding as `0000-01-01 00:00:00`, not
            // the Unix epoch: `days_from_civil(0, 1, 1) * 86_400 * 1_000_000`.
            ParamValue::Value(Value::Timestamp(
                days_from_civil(0, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC,
            )),
            ParamValue::Value(Value::Timestamp(1_700_000_000_000_000)),
            ParamValue::Value(Value::Timestamp(1_700_000_000_123_456)),
        ];
        assert_eq!(decoded.values, expected);
    }

    #[test]
    fn decode_execute_null_bitmap_offset_zero() {
        // Two params: first NULL, second a TINY value. Offset 0 means bit 0 (not bit 2) marks
        // the first parameter.
        let params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![
            (MYSQL_TYPE_TINY, false, None),
            (MYSQL_TYPE_TINY, false, Some(vec![5])),
        ];
        let payload = build_execute_payload(1, &params);
        // stmt_id(4) + flags(1) + iteration_count(4) = 9 bytes header, then the 1-byte bitmap.
        assert_eq!(
            payload[9], 0b0000_0001,
            "bit 0 must mark the first parameter NULL"
        );
        let decoded = decode_execute(&payload, 2, None, &[false, false]).unwrap();
        assert_eq!(
            decoded.values,
            vec![
                ParamValue::Value(Value::Null),
                ParamValue::Value(Value::Int32(5))
            ]
        );
    }

    #[test]
    fn decode_execute_flag_zero_uses_cache() {
        let params: Vec<(u8, bool, Option<Vec<u8>>)> =
            vec![(MYSQL_TYPE_LONG, false, Some(42i32.to_le_bytes().to_vec()))];
        let full_payload = build_execute_payload(1, &params);
        let first = decode_execute(&full_payload, 1, None, &[false]).unwrap();
        assert_eq!(
            first.types,
            vec![ParamType {
                mysql_type: MYSQL_TYPE_LONG,
                unsigned: false
            }]
        );

        // A second EXECUTE with new_params_bound_flag = 0: header + bitmap + flag byte, no
        // type bytes, then the value.
        let mut second_payload = Vec::new();
        second_payload.extend_from_slice(&1u32.to_le_bytes());
        second_payload.push(CURSOR_TYPE_NO_CURSOR);
        second_payload.extend_from_slice(&1u32.to_le_bytes());
        second_payload.push(0); // bitmap: not null
        second_payload.push(0); // new_params_bound_flag = 0
        second_payload.extend_from_slice(&99i32.to_le_bytes());

        let cached = first.types.clone();
        let second = decode_execute(&second_payload, 1, Some(&cached), &[false]).unwrap();
        assert_eq!(second.types, cached);
        assert_eq!(second.values, vec![ParamValue::Value(Value::Int64(99))]);

        // Flag = 0 with no cache is a clean error.
        let err = decode_execute(&second_payload, 1, None, &[false]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Flag = 0 with a mismatched-length cache is also a clean error.
        let err2 = decode_execute(&second_payload, 1, Some(&[]), &[false]).unwrap_err();
        assert_eq!(err2.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_execute_zero_params() {
        let payload = build_execute_payload(5, &[]);
        assert_eq!(payload.len(), 9);
        let decoded = decode_execute(&payload, 0, None, &[]).unwrap();
        assert_eq!(decoded.stmt_id, 5);
        assert!(decoded.types.is_empty());
        assert!(decoded.values.is_empty());
    }

    #[test]
    fn decode_execute_unsigned_longlong_boundary() {
        let ok_params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![(
            MYSQL_TYPE_LONGLONG,
            true,
            Some((i64::MAX as u64).to_le_bytes().to_vec()),
        )];
        let ok_payload = build_execute_payload(1, &ok_params);
        assert_eq!(
            decode_execute(&ok_payload, 1, None, &[false])
                .unwrap()
                .values,
            vec![ParamValue::Value(Value::Int64(i64::MAX))]
        );

        let over_params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![(
            MYSQL_TYPE_LONGLONG,
            true,
            Some((i64::MAX as u64 + 1).to_le_bytes().to_vec()),
        )];
        let over_payload = build_execute_payload(1, &over_params);
        let err = decode_execute(&over_payload, 1, None, &[false]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("i64::MAX"));
    }

    #[test]
    fn decode_execute_rejects_unsupported_cursor_flag() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(1); // CURSOR_TYPE_READ_ONLY, not supported
        payload.extend_from_slice(&1u32.to_le_bytes());
        let err = decode_execute(&payload, 0, None, &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_execute_rejects_time_and_unknown_types_by_name() {
        for (type_code, needle) in [(MYSQL_TYPE_TIME, "TIME"), (0x99, "0x99")] {
            let params: Vec<(u8, bool, Option<Vec<u8>>)> =
                vec![(type_code, false, Some(vec![0; 8]))];
            let payload = build_execute_payload(1, &params);
            let err = decode_execute(&payload, 1, None, &[false]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(err.to_string().contains(needle), "{err}");
        }
    }

    #[test]
    fn decode_execute_rejects_invalid_decimal_text() {
        let params: Vec<(u8, bool, Option<Vec<u8>>)> =
            vec![(MYSQL_TYPE_NEWDECIMAL, false, Some(lenenc_bytes(b"12.3.4")))];
        let payload = build_execute_payload(1, &params);
        assert!(decode_execute(&payload, 1, None, &[false]).is_err());

        let params2: Vec<(u8, bool, Option<Vec<u8>>)> =
            vec![(MYSQL_TYPE_NEWDECIMAL, false, Some(lenenc_bytes(b"abc")))];
        let payload2 = build_execute_payload(1, &params2);
        assert!(decode_execute(&payload2, 1, None, &[false]).is_err());

        let params3: Vec<(u8, bool, Option<Vec<u8>>)> =
            vec![(MYSQL_TYPE_NEWDECIMAL, false, Some(lenenc_bytes(b"")))];
        let payload3 = build_execute_payload(1, &params3);
        assert!(decode_execute(&payload3, 1, None, &[false]).is_err());
    }

    #[test]
    fn decode_execute_var_string_invalid_utf8_decodes_as_bytes_not_lossy_string() {
        // A bound Rust `Vec<u8>` that happens not to be valid UTF-8 arrives as
        // `MYSQL_TYPE_VAR_STRING` on the wire (see the module doc comment): this must decode as
        // `Value::Bytes` with every byte intact, never as a lossily-replaced `Value::String`.
        let invalid_utf8 = vec![0x66, 0x6f, 0xff, 0x6f]; // "fo\xFFo"
        let params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![(
            MYSQL_TYPE_VAR_STRING,
            false,
            Some(lenenc_bytes(&invalid_utf8)),
        )];
        let payload = build_execute_payload(1, &params);
        let decoded = decode_execute(&payload, 1, None, &[false]).unwrap();
        assert_eq!(
            decoded.values,
            vec![ParamValue::Value(Value::Bytes(invalid_utf8))]
        );
    }

    #[test]
    fn decode_execute_var_string_valid_utf8_still_decodes_as_string() {
        let params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![(
            MYSQL_TYPE_VAR_STRING,
            false,
            Some(lenenc_bytes("héllo".as_bytes())),
        )];
        let payload = build_execute_payload(1, &params);
        let decoded = decode_execute(&payload, 1, None, &[false]).unwrap();
        assert_eq!(
            decoded.values,
            vec![ParamValue::Value(Value::String("héllo".into()))]
        );
    }

    #[test]
    fn decode_execute_long_data_placeholder() {
        // Non-null BLOB param whose value bytes are absent from the payload because they were
        // already accumulated via COM_STMT_SEND_LONG_DATA. Built manually: bitmap says "not
        // null", type is BLOB, but no value bytes follow at all (as the real protocol behaves
        // for a long-data parameter — see the module doc comment).
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(CURSOR_TYPE_NO_CURSOR);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(0); // bitmap: not null
        payload.push(1); // new_params_bound_flag
        payload.push(MYSQL_TYPE_BLOB);
        payload.push(0);
        // No value bytes for this parameter.

        let decoded = decode_execute(&payload, 1, None, &[true]).unwrap();
        assert_eq!(decoded.values, vec![ParamValue::LongDataPlaceholder]);
    }

    #[test]
    fn decode_execute_null_bit_wins_over_long_data_pending() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(CURSOR_TYPE_NO_CURSOR);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.push(1); // bitmap: null
        payload.push(1); // new_params_bound_flag
        payload.push(MYSQL_TYPE_BLOB);
        payload.push(0);

        let decoded = decode_execute(&payload, 1, None, &[true]).unwrap();
        assert_eq!(decoded.values, vec![ParamValue::Value(Value::Null)]);
    }

    #[test]
    fn decode_execute_rejects_wrong_long_data_pending_length() {
        let payload = build_execute_payload(1, &[(MYSQL_TYPE_LONG, false, Some(vec![0; 4]))]);
        let err = decode_execute(&payload, 1, None, &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_execute_truncated_and_garbage_payloads_never_panic() {
        let params: Vec<(u8, bool, Option<Vec<u8>>)> = vec![(
            MYSQL_TYPE_VAR_STRING,
            false,
            Some(lenenc_bytes(b"hello world")),
        )];
        let full = build_execute_payload(1, &params);
        for len in 0..full.len() {
            let truncated = &full[..len];
            let result = decode_execute(truncated, 1, None, &[false]);
            assert!(result.is_err(), "truncated to {len} bytes should error");
        }
        // Completely random short buffers must never panic either.
        for buf in [
            vec![],
            vec![0xff; 3],
            vec![0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00],
        ] {
            let _ = decode_execute(&buf, 1, None, &[false]);
        }
    }

    #[test]
    fn decode_execute_rejects_trailing_bytes() {
        let mut payload = build_execute_payload(1, &[(MYSQL_TYPE_LONG, false, Some(vec![0; 4]))]);
        payload.push(0xaa);
        let err = decode_execute(&payload, 1, None, &[false]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // -------------------------------------------------------------------------------------
    // encode_stmt_prepare_ok
    // -------------------------------------------------------------------------------------

    #[test]
    fn stmt_prepare_ok_round_trip() {
        let payload = encode_stmt_prepare_ok(42, 3, 2, 0);
        assert_eq!(payload.len(), 12);
        assert_eq!(payload[0], OK_HEADER);
        let decoded = decode_stmt_prepare_ok(&payload).unwrap();
        assert_eq!(
            decoded,
            StmtPrepareOk {
                stmt_id: 42,
                num_columns: 3,
                num_params: 2,
                warning_count: 0,
            }
        );
        assert!(decode_stmt_prepare_ok(&payload[..5]).is_err());
        let mut bad_header = payload.clone();
        bad_header[0] = 0xff;
        assert!(decode_stmt_prepare_ok(&bad_header).is_err());
    }

    // -------------------------------------------------------------------------------------
    // encode_binary_row
    // -------------------------------------------------------------------------------------

    #[test]
    fn binary_row_null_bitmap_offset_two() {
        let columns = vec![
            col("a", DataType::Int32, true),
            col("b", DataType::Int32, true),
        ];
        // First column NULL, second not: offset 2 means bit index 2 (byte 0, bit 2) marks
        // column 0, not bit 0.
        let row = Row::new(vec![Value::Null, Value::Int32(7)]);
        let payload = encode_binary_row(&row, &columns).unwrap();
        assert_eq!(payload[0], 0x00);
        assert_eq!(payload[1], 0b0000_0100, "bit 2 must mark column 0 NULL");
        assert_eq!(decode_binary_row(&payload, &columns).unwrap(), row);
    }

    #[test]
    fn binary_row_round_trip_all_types() {
        let columns = vec![
            col("bo", DataType::Bool, false),
            col("i32", DataType::Int32, false),
            col("i64", DataType::Int64, false),
            col("f64", DataType::Float64, false),
            col("s", DataType::String, false),
            col("by", DataType::Bytes, false),
            col("ts", DataType::Timestamp, false),
        ];
        let row = Row::new(vec![
            Value::Bool(true),
            Value::Int32(-7),
            Value::Int64(i64::MIN),
            Value::Float64(-1.5e300),
            Value::String("héllo".into()),
            Value::Bytes(vec![0x00, 0xff]),
            Value::Timestamp(1_700_000_000_123_456),
        ]);
        let payload = encode_binary_row(&row, &columns).unwrap();
        assert_eq!(decode_binary_row(&payload, &columns).unwrap(), row);
    }

    #[test]
    fn binary_row_all_null() {
        let columns = vec![
            col("a", DataType::Int32, true),
            col("b", DataType::String, true),
        ];
        let row = Row::new(vec![Value::Null, Value::Null]);
        let payload = encode_binary_row(&row, &columns).unwrap();
        // header(1) + bitmap((2+9)/8=1 byte) with both bits set (offset 2: bits 2 and 3).
        assert_eq!(payload, vec![0x00, 0b0000_1100]);
        assert_eq!(decode_binary_row(&payload, &columns).unwrap(), row);
    }

    #[test]
    fn binary_datetime_zero_four_seven_eleven_byte_forms_round_trip() {
        for micros in [
            0i64,                  // 1970-01-01 00:00:00.000000 -> 4-byte form
            1_700_000_000_000_000, // has a nonzero time -> 7-byte form
            1_700_000_000_123_456, // has a fractional second -> 11-byte form
            -1,                    // 1969-12-31 23:59:59.999999 -> 11-byte form
        ] {
            let encoded = encode_binary_datetime(micros).unwrap();
            let mut pos = 0;
            let decoded = decode_binary_datetime(&encoded, &mut pos).unwrap();
            assert_eq!(decoded, micros, "micros={micros}");
            assert_eq!(pos, encoded.len());
        }
        // The 0-byte zero-date form decodes without panicking (module doc comment), as
        // `0000-01-01 00:00:00` (month/day clamped to 1), not the Unix epoch.
        let mut pos = 0;
        assert_eq!(
            decode_binary_datetime(&[0], &mut pos).unwrap(),
            days_from_civil(0, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC
        );
        assert_eq!(pos, 1);
    }

    // -------------------------------------------------------------------------------------
    // Finding 6 of the Phase 11 fix pass: binary DATE/DATETIME/TIMESTAMP range checks.
    // -------------------------------------------------------------------------------------

    /// Builds an 11-byte binary datetime payload (the length byte plus every field) for a test
    /// to corrupt one field of.
    fn datetime_11(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Vec<u8> {
        let mut b = vec![11u8];
        b.extend_from_slice(&year.to_le_bytes());
        b.extend_from_slice(&[month, day, hour, minute, second]);
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    #[test]
    fn decode_binary_datetime_rejects_invalid_calendar_fields() {
        // A valid baseline first, to confirm the harness itself is right.
        let ok = datetime_11(2023, 11, 14, 22, 13, 20);
        assert!(decode_binary_datetime(&ok, &mut 0).is_ok());

        let cases: Vec<(Vec<u8>, &str)> = vec![
            (datetime_11(2023, 0, 14, 0, 0, 0), "month"), // month 0 without day 0: not the sentinel
            (datetime_11(2023, 13, 14, 0, 0, 0), "month"),
            (datetime_11(2023, 4, 31, 0, 0, 0), "day"), // April has 30 days
            (datetime_11(2023, 2, 29, 0, 0, 0), "day"), // 2023 is not a leap year
            (datetime_11(2023, 2, 0, 0, 0, 0), "day"),  // day 0 without month 0: not the sentinel
            (datetime_11(2023, 1, 32, 0, 0, 0), "day"),
            (datetime_11(2023, 1, 1, 24, 0, 0), "hour"),
            (datetime_11(2023, 1, 1, 0, 60, 0), "minute"),
            (datetime_11(2023, 1, 1, 0, 0, 60), "second"),
        ];
        for (payload, needle) in cases {
            let err = decode_binary_datetime(&payload, &mut 0).unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains(needle),
                "expected {needle:?} in error for {payload:?}, got {err}"
            );
        }

        // 2024 is a leap year: Feb 29 is valid.
        assert!(decode_binary_datetime(&datetime_11(2024, 2, 29, 0, 0, 0), &mut 0).is_ok());

        // Out-of-range microseconds (11-byte form).
        let mut bad_micros = vec![11u8];
        bad_micros.extend_from_slice(&2023u16.to_le_bytes());
        bad_micros.extend_from_slice(&[1, 1, 0, 0, 0]);
        bad_micros.extend_from_slice(&1_000_000u32.to_le_bytes());
        let err = decode_binary_datetime(&bad_micros, &mut 0).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("microsecond"));

        // A year past the protocol's 0-9999 range.
        let mut bad_year = vec![4u8];
        bad_year.extend_from_slice(&10_000u16.to_le_bytes());
        bad_year.extend_from_slice(&[1, 1]);
        let err = decode_binary_datetime(&bad_year, &mut 0).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("year"));
    }

    #[test]
    fn decode_binary_datetime_zero_month_and_day_together_is_the_sentinel() {
        // month == 0 && day == 0 together (not the genuine 0-byte form) is still the documented
        // zero-date sentinel, not a calendar error.
        let payload = datetime_11(0, 0, 0, 1, 2, 3);
        let decoded = decode_binary_datetime(&payload, &mut 0).unwrap();
        let (hh, mm, ss): (i64, i64, i64) = (1, 2, 3);
        let expected = days_from_civil(0, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC
            + (hh * 3600 + mm * 60 + ss) * MICROS_PER_SEC;
        assert_eq!(decoded, expected);
    }

    #[test]
    fn encode_binary_datetime_rejects_year_outside_0_to_9999() {
        let too_late = days_from_civil(10_000, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC;
        let err = encode_binary_datetime(too_late).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("year"));

        let too_early = days_from_civil(-1, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC;
        let err = encode_binary_datetime(too_early).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("year"));

        // The boundary years still succeed.
        assert!(
            encode_binary_datetime(days_from_civil(0, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC)
                .is_ok()
        );
        assert!(encode_binary_datetime(
            days_from_civil(9999, 12, 31) * SECS_PER_DAY * MICROS_PER_SEC
        )
        .is_ok());
    }

    #[test]
    fn encode_execute_request_propagates_out_of_range_timestamp_error() {
        let too_late = days_from_civil(10_000, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC;
        assert!(encode_execute_request(1, &[Value::Timestamp(too_late)]).is_err());
        assert!(encode_execute_request(1, &[Value::Timestamp(0)]).is_ok());
    }

    #[test]
    fn encode_binary_row_propagates_out_of_range_timestamp_error() {
        let columns = vec![col("ts", DataType::Timestamp, false)];
        let too_late = days_from_civil(10_000, 1, 1) * SECS_PER_DAY * MICROS_PER_SEC;
        let row = Row::new(vec![Value::Timestamp(too_late)]);
        assert!(encode_binary_row(&row, &columns).is_err());
    }
}
