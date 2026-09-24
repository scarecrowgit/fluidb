//! Schema-aware parsing, formatting, and envelope conversion for CSV and JSONLines.
//!
//! # Enforced Semantics
//! - **CSV**: When headers are enabled (`has_header = true`), a header line is required and must
//!   contain exact schema column names once (order may vary). When headers are disabled
//!   (`has_header = false`), fields are interpreted in schema declaration order, requiring
//!   exactly `schema.len()` fields. Configurable delimiter, `\N` represents `NULL`, opaque bytes
//!   formatted/parsed as lowercase hex strings.
//! - **JSONLines**: One JSON object per line, exact schema fields required, JSON `null` represents
//!   `NULL`, opaque bytes formatted/parsed as hex strings, timestamp formatted/parsed as integer microseconds.
//! - **Limits**: Enforces bounded line lengths ([`MAX_LINE_BYTES`]) and field lengths ([`MAX_FIELD_BYTES`]).

use std::collections::HashSet;
use std::fmt::Write;

use htap_common::types::{
    check_decimal_precision, parse_date_to_timestamp_micros, parse_decimal_text,
};
use htap_common::{ColumnDef, DataType, HtapError, Result, Row, Schema, Value};

use crate::job::DataFormat;

/// Maximum allowable line/record length (16 MiB) to guard against unbounded allocation.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum allowable individual field size (16 MiB).
pub const MAX_FIELD_BYTES: usize = 16 * 1024 * 1024;

/// Encode byte slice into lowercase hexadecimal string.
pub fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Decode hexadecimal string into byte vector, allowing optional leading `0x` or `0X`.
pub fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let raw = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);

    if !raw.len().is_multiple_of(2) {
        return Err(HtapError::InvalidArgument(format!(
            "invalid hex byte string length {}",
            raw.len()
        )));
    }

    let mut bytes = Vec::with_capacity(raw.len() / 2);
    for i in (0..raw.len()).step_by(2) {
        let byte = u8::from_str_radix(&raw[i..i + 2], 16).map_err(|e| {
            HtapError::InvalidArgument(format!("invalid hex character in '{s}': {e}"))
        })?;
        bytes.push(byte);
    }

    Ok(bytes)
}

/// Parse timestamp text as integer microseconds, an ISO calendar date, or a datetime.
///
/// Datetimes use `YYYY-MM-DD HH:MM:SS` and are interpreted as UTC.
fn parse_timestamp_text(text: &str) -> Result<i64> {
    if let Ok(micros) = text.parse::<i64>() {
        return Ok(micros);
    }

    if let Some((date_text, time_text)) = text.split_once(' ') {
        if date_text.is_empty() || time_text.is_empty() || time_text.contains(' ') {
            return Err(HtapError::InvalidArgument(format!(
                "invalid datetime format '{text}', expected YYYY-MM-DD HH:MM:SS"
            )));
        }

        let date_micros = parse_date_to_timestamp_micros(date_text)?;
        let mut parts = time_text.split(':');
        let (hours_text, minutes_text, seconds_text) =
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(hours), Some(minutes), Some(seconds), None)
                    if [hours, minutes, seconds].iter().all(|part| {
                        part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit())
                    }) =>
                {
                    (hours, minutes, seconds)
                }
                _ => {
                    return Err(HtapError::InvalidArgument(format!(
                        "invalid datetime time component in '{text}', expected HH:MM:SS"
                    )))
                }
            };

        let hours = hours_text.parse::<i64>().map_err(|_| {
            HtapError::InvalidArgument(format!(
                "invalid datetime time component in '{text}', expected HH:MM:SS"
            ))
        })?;
        let minutes = minutes_text.parse::<i64>().map_err(|_| {
            HtapError::InvalidArgument(format!(
                "invalid datetime time component in '{text}', expected HH:MM:SS"
            ))
        })?;
        let seconds = seconds_text.parse::<i64>().map_err(|_| {
            HtapError::InvalidArgument(format!(
                "invalid datetime time component in '{text}', expected HH:MM:SS"
            ))
        })?;

        if !(0..24).contains(&hours) || !(0..60).contains(&minutes) || !(0..60).contains(&seconds) {
            return Err(HtapError::InvalidArgument(format!(
                "invalid datetime time component in '{text}', expected HH:MM:SS"
            )));
        }

        let time_micros = (hours * 3600 + minutes * 60 + seconds) * 1_000_000;
        return date_micros.checked_add(time_micros).ok_or_else(|| {
            HtapError::InvalidArgument(format!("datetime '{text}' is out of timestamp range"))
        });
    }

    parse_date_to_timestamp_micros(text).map_err(|e| {
        HtapError::InvalidArgument(format!(
            "cannot parse '{text}' as timestamp microseconds, calendar date, or datetime: {e}"
        ))
    })
}

/// Column index permutation map matching CSV record positions to table schema columns.
///
/// When CSV headers are present, positions are mapped dynamically by column name.
/// When headers are disabled (`has_header = false`), fields are mapped in table schema
/// declaration order (1:1 identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvHeaderMap {
    /// Maps schema column index -> index in CSV record.
    schema_to_csv: Vec<usize>,
}

impl CsvHeaderMap {
    /// Build header mapping by verifying that `headers` contains exactly the column names
    /// defined by `schema` without duplicates or missing entries.
    pub fn parse(schema: &Schema, headers: &csv::StringRecord) -> Result<Self> {
        if headers.len() != schema.len() {
            return Err(HtapError::InvalidArgument(format!(
                "CSV header column count ({}) does not match schema column count ({})",
                headers.len(),
                schema.len()
            )));
        }

        let mut seen_headers = HashSet::with_capacity(headers.len());
        for col_name in headers {
            if !seen_headers.insert(col_name) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate column name in CSV header: '{col_name}'"
                )));
            }
            if schema.column_index(col_name).is_none() {
                return Err(HtapError::InvalidArgument(format!(
                    "unknown column name in CSV header: '{col_name}'"
                )));
            }
        }

        let mut schema_to_csv = Vec::with_capacity(schema.len());
        for col in schema.columns() {
            let pos = headers.iter().position(|h| h == col.name).ok_or_else(|| {
                HtapError::InvalidArgument(format!(
                    "required column '{}' missing from CSV header",
                    col.name
                ))
            })?;
            schema_to_csv.push(pos);
        }

        Ok(Self { schema_to_csv })
    }

    /// Identity permutation mapping schema column `i` to CSV record index `i`.
    ///
    /// Used for headerless CSV input where fields are interpreted in schema declaration order,
    /// requiring exactly `len` fields.
    pub fn identity(len: usize) -> Self {
        Self {
            schema_to_csv: (0..len).collect(),
        }
    }

    /// Build header mapping matching schema declaration order for headerless CSV.
    ///
    /// Interprets CSV record fields in schema declaration order, requiring exactly
    /// `schema.len()` fields.
    pub fn from_schema(schema: &Schema) -> Self {
        Self::identity(schema.len())
    }

    /// Retrieve the CSV record field index for a schema column index.
    #[inline]
    pub fn schema_to_csv(&self, schema_idx: usize) -> usize {
        self.schema_to_csv[schema_idx]
    }
}

/// Parse a single string field from CSV into a typed [`Value`].
///
/// Implements `\N` as SQL NULL, and parses types according to [`DataType`].
pub fn parse_csv_field(col: &ColumnDef, field: &str) -> Result<Value> {
    if field.len() > MAX_FIELD_BYTES {
        return Err(HtapError::InvalidArgument(format!(
            "field for column '{}' exceeds maximum allowed size {}",
            col.name, MAX_FIELD_BYTES
        )));
    }

    if field == r"\N" {
        if col.nullable {
            return Ok(Value::Null);
        }
        return Err(HtapError::InvalidArgument(format!(
            "column '{}' is not nullable but received NULL (\\N)",
            col.name
        )));
    }

    match col.data_type {
        DataType::Bool => {
            if field.eq_ignore_ascii_case("true") || field == "1" || field.eq_ignore_ascii_case("t")
            {
                Ok(Value::Bool(true))
            } else if field.eq_ignore_ascii_case("false")
                || field == "0"
                || field.eq_ignore_ascii_case("f")
            {
                Ok(Value::Bool(false))
            } else {
                Err(HtapError::InvalidArgument(format!(
                    "cannot parse '{}' as boolean for column '{}'",
                    field, col.name
                )))
            }
        }
        DataType::Int32 => field.parse::<i32>().map(Value::Int32).map_err(|e| {
            HtapError::InvalidArgument(format!(
                "cannot parse '{}' as int32 for column '{}': {e}",
                field, col.name
            ))
        }),
        DataType::Int64 => field.parse::<i64>().map(Value::Int64).map_err(|e| {
            HtapError::InvalidArgument(format!(
                "cannot parse '{}' as int64 for column '{}': {e}",
                field, col.name
            ))
        }),
        DataType::Float64 => field.parse::<f64>().map(Value::Float64).map_err(|e| {
            HtapError::InvalidArgument(format!(
                "cannot parse '{}' as float64 for column '{}': {e}",
                field, col.name
            ))
        }),
        DataType::String => Ok(Value::String(field.to_string())),
        DataType::Bytes => decode_hex(field).map(Value::Bytes).map_err(|e| {
            HtapError::InvalidArgument(format!(
                "cannot parse '{}' as hex bytes for column '{}': {e}",
                field, col.name
            ))
        }),
        DataType::Timestamp => parse_timestamp_text(field)
            .map(Value::Timestamp)
            .map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "cannot parse '{}' as timestamp for column '{}': {e}",
                    field, col.name
                ))
            }),
        DataType::Decimal { precision, scale } => {
            let value = parse_decimal_text(field, scale).map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "cannot parse '{}' as decimal for column '{}': {e}",
                    field, col.name
                ))
            })?;
            let value = i64::try_from(value).map_err(|_| {
                HtapError::InvalidArgument(format!(
                    "decimal value '{}' overflows i64 for column '{}'",
                    field, col.name
                ))
            })?;
            let value = check_decimal_precision(value, precision, scale)?;

            Ok(Value::Decimal {
                value,
                precision,
                scale,
            })
        }
    }
}

/// Decode a CSV string record into a strongly-typed [`Row`] matching [`Schema`].
///
/// Requires exactly `schema.len()` fields in the record. Maps fields into schema
/// columns using `header_map` (either dynamic header mapping or schema declaration order).
pub fn decode_csv_record(
    schema: &Schema,
    header_map: &CsvHeaderMap,
    record: &csv::StringRecord,
) -> Result<Row> {
    if record.len() != schema.len() {
        return Err(HtapError::InvalidArgument(format!(
            "CSV record field count ({}) does not match schema width ({})",
            record.len(),
            schema.len()
        )));
    }

    let mut total_bytes = 0usize;
    let mut values = Vec::with_capacity(schema.len());

    for (schema_idx, col) in schema.columns().iter().enumerate() {
        let csv_idx = header_map.schema_to_csv(schema_idx);
        let field = record.get(csv_idx).ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "record missing field for column '{}' at index {}",
                col.name, csv_idx
            ))
        })?;

        total_bytes += field.len();
        if total_bytes > MAX_LINE_BYTES {
            return Err(HtapError::InvalidArgument(format!(
                "CSV record total byte size exceeds maximum line limit {}",
                MAX_LINE_BYTES
            )));
        }

        let val = parse_csv_field(col, field)?;
        values.push(val);
    }

    Ok(Row::new(values))
}

/// Format a single [`Value`] as a CSV field string according to HTAP conventions.
pub fn format_csv_field(val: &Value) -> Result<String> {
    match val {
        Value::Null => Ok(r"\N".to_string()),
        Value::Bool(b) => Ok(if *b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        Value::Int32(v) => Ok(v.to_string()),
        Value::Int64(v) => Ok(v.to_string()),
        Value::Float64(v) => Ok(v.to_string()),
        Value::String(s) => Ok(s.clone()),
        Value::Bytes(b) => Ok(encode_hex(b)),
        Value::Timestamp(v) => Ok(v.to_string()),
        Value::Decimal { .. } => Ok(val.to_string()),
    }
}

/// Generate a CSV header record matching schema column declaration order.
pub fn encode_csv_header(schema: &Schema) -> csv::StringRecord {
    let mut rec = csv::StringRecord::with_capacity(schema.len(), schema.len());
    for col in schema.columns() {
        rec.push_field(&col.name);
    }
    rec
}

/// Format a [`Row`] as a CSV string record in schema declaration order.
pub fn encode_csv_record(schema: &Schema, row: &Row) -> Result<csv::StringRecord> {
    let mut rec = csv::StringRecord::with_capacity(schema.len(), schema.len());
    for val in row.values() {
        rec.push_field(&format_csv_field(val)?);
    }
    Ok(rec)
}

/// Parse a single JSON value into a strongly-typed [`Value`] matching [`ColumnDef`].
pub fn parse_json_value(col: &ColumnDef, json_val: &serde_json::Value) -> Result<Value> {
    if json_val.is_null() {
        if col.nullable {
            return Ok(Value::Null);
        }
        return Err(HtapError::InvalidArgument(format!(
            "column '{}' is not nullable but received null",
            col.name
        )));
    }

    match col.data_type {
        DataType::Bool => match json_val {
            serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
            _ => Err(HtapError::InvalidArgument(format!(
                "expected boolean for column '{}', found {}",
                col.name, json_val
            ))),
        },
        DataType::Int32 => match json_val {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    let i32_val = i32::try_from(i).map_err(|_| {
                        HtapError::InvalidArgument(format!(
                            "int32 value {i} out of range for column '{}'",
                            col.name
                        ))
                    })?;
                    Ok(Value::Int32(i32_val))
                } else {
                    Err(HtapError::InvalidArgument(format!(
                        "invalid integer number for column '{}'",
                        col.name
                    )))
                }
            }
            serde_json::Value::String(s) => s.parse::<i32>().map(Value::Int32).map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "cannot parse '{}' as int32 for column '{}': {e}",
                    s, col.name
                ))
            }),
            _ => Err(HtapError::InvalidArgument(format!(
                "expected number for column '{}'",
                col.name
            ))),
        },
        DataType::Int64 => match json_val {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Value::Int64(i))
                } else {
                    Err(HtapError::InvalidArgument(format!(
                        "invalid int64 number for column '{}'",
                        col.name
                    )))
                }
            }
            serde_json::Value::String(s) => s.parse::<i64>().map(Value::Int64).map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "cannot parse '{}' as int64 for column '{}': {e}",
                    s, col.name
                ))
            }),
            _ => Err(HtapError::InvalidArgument(format!(
                "expected number for column '{}'",
                col.name
            ))),
        },
        DataType::Float64 => match json_val {
            serde_json::Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    Ok(Value::Float64(f))
                } else {
                    Err(HtapError::InvalidArgument(format!(
                        "invalid float64 number for column '{}'",
                        col.name
                    )))
                }
            }
            serde_json::Value::String(s) => s.parse::<f64>().map(Value::Float64).map_err(|e| {
                HtapError::InvalidArgument(format!(
                    "cannot parse '{}' as float64 for column '{}': {e}",
                    s, col.name
                ))
            }),
            _ => Err(HtapError::InvalidArgument(format!(
                "expected number for column '{}'",
                col.name
            ))),
        },
        DataType::String => match json_val {
            serde_json::Value::String(s) => {
                if s.len() > MAX_FIELD_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "string for column '{}' exceeds maximum allowed size {}",
                        col.name, MAX_FIELD_BYTES
                    )));
                }
                Ok(Value::String(s.clone()))
            }
            _ => Err(HtapError::InvalidArgument(format!(
                "expected string for column '{}'",
                col.name
            ))),
        },
        DataType::Bytes => match json_val {
            serde_json::Value::String(s) => {
                if s.len() > MAX_FIELD_BYTES * 2 {
                    return Err(HtapError::InvalidArgument(format!(
                        "hex bytes for column '{}' exceeds maximum allowed size",
                        col.name
                    )));
                }
                decode_hex(s).map(Value::Bytes).map_err(|e| {
                    HtapError::InvalidArgument(format!(
                        "invalid hex byte string for column '{}': {e}",
                        col.name
                    ))
                })
            }
            _ => Err(HtapError::InvalidArgument(format!(
                "expected hex string for bytes column '{}'",
                col.name
            ))),
        },
        DataType::Timestamp => match json_val {
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Value::Timestamp(i))
                } else {
                    Err(HtapError::InvalidArgument(format!(
                        "invalid timestamp number for column '{}'",
                        col.name
                    )))
                }
            }
            serde_json::Value::String(s) => {
                parse_timestamp_text(s).map(Value::Timestamp).map_err(|e| {
                    HtapError::InvalidArgument(format!(
                        "cannot parse '{}' as timestamp for column '{}': {e}",
                        s, col.name
                    ))
                })
            }
            _ => Err(HtapError::InvalidArgument(format!(
                "expected number or string for timestamp column '{}'",
                col.name
            ))),
        },
        DataType::Decimal { precision, scale } => match json_val {
            serde_json::Value::String(s) => {
                if s.len() > MAX_FIELD_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "decimal string for column '{}' exceeds maximum allowed size {}",
                        col.name, MAX_FIELD_BYTES
                    )));
                }

                let value = parse_decimal_text(s, scale).map_err(|e| {
                    HtapError::InvalidArgument(format!(
                        "cannot parse '{}' as decimal for column '{}': {e}",
                        s, col.name
                    ))
                })?;
                let value = i64::try_from(value).map_err(|_| {
                    HtapError::InvalidArgument(format!(
                        "decimal value '{}' overflows i64 for column '{}'",
                        s, col.name
                    ))
                })?;
                let value = check_decimal_precision(value, precision, scale)?;

                Ok(Value::Decimal {
                    value,
                    precision,
                    scale,
                })
            }
            serde_json::Value::Number(_) => Err(HtapError::InvalidArgument(format!(
                "decimal values in JSON for column '{}' must be strings, not numbers",
                col.name
            ))),
            _ => Err(HtapError::InvalidArgument(format!(
                "expected decimal string for column '{}'",
                col.name
            ))),
        },
    }
}

/// Decode a JSONLines line into a strongly-typed [`Row`] matching [`Schema`].
pub fn decode_json_line(schema: &Schema, line: &str) -> Result<Row> {
    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
    if trimmed.is_empty() {
        return Err(HtapError::InvalidArgument("empty JSON line".into()));
    }
    if trimmed.len() > MAX_LINE_BYTES {
        return Err(HtapError::InvalidArgument(format!(
            "JSON line length {} exceeds maximum limit {}",
            trimmed.len(),
            MAX_LINE_BYTES
        )));
    }

    let parsed: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| HtapError::InvalidArgument(format!("malformed JSON: {e}")))?;

    let map = match parsed {
        serde_json::Value::Object(m) => m,
        _ => {
            return Err(HtapError::InvalidArgument(
                "JSON line must be a JSON object".into(),
            ))
        }
    };

    if map.len() != schema.len() {
        return Err(HtapError::InvalidArgument(format!(
            "JSON object field count ({}) does not match schema width ({})",
            map.len(),
            schema.len()
        )));
    }

    let mut values = Vec::with_capacity(schema.len());
    for col in schema.columns() {
        let json_val = map.get(&col.name).ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "missing required field '{}' in JSON object",
                col.name
            ))
        })?;
        let val = parse_json_value(col, json_val)?;
        values.push(val);
    }

    Ok(Row::new(values))
}

/// Format a single [`Value`] as a [`serde_json::Value`] according to HTAP conventions.
pub fn format_json_value(val: &Value) -> Result<serde_json::Value> {
    match val {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Int32(v) => Ok(serde_json::json!(*v)),
        Value::Int64(v) => Ok(serde_json::json!(*v)),
        Value::Float64(v) => Ok(if let Some(n) = serde_json::Number::from_f64(*v) {
            serde_json::Value::Number(n)
        } else {
            serde_json::Value::String(v.to_string())
        }),
        Value::String(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Bytes(b) => Ok(serde_json::Value::String(encode_hex(b))),
        Value::Timestamp(v) => Ok(serde_json::json!(*v)),
        Value::Decimal { .. } => Ok(serde_json::Value::String(val.to_string())),
    }
}

/// Format a [`Row`] as a single JSONLines object string matching [`Schema`].
pub fn encode_json_line(schema: &Schema, row: &Row) -> Result<String> {
    if row.len() != schema.len() {
        return Err(HtapError::InvalidArgument(format!(
            "row length ({}) does not match schema width ({})",
            row.len(),
            schema.len()
        )));
    }

    let mut out = String::from("{");
    for (i, (col, val)) in schema.columns().iter().zip(row.values().iter()).enumerate() {
        if i > 0 {
            out.push(',');
        }
        let key_json = serde_json::to_string(&col.name)
            .map_err(|e| HtapError::Internal(format!("failed to serialize key: {e}")))?;
        let val_json = serde_json::to_string(&format_json_value(val)?)
            .map_err(|e| HtapError::Internal(format!("failed to serialize value: {e}")))?;
        out.push_str(&key_json);
        out.push(':');
        out.push_str(&val_json);
    }
    out.push('}');
    Ok(out)
}

/// Generic record decoder dispatching on [`DataFormat`].
pub fn decode_record(schema: &Schema, format: DataFormat, data: &[u8]) -> Result<Row> {
    match format {
        DataFormat::Csv => {
            let s = std::str::from_utf8(data)
                .map_err(|e| HtapError::InvalidArgument(format!("non-UTF8 CSV record: {e}")))?;
            let mut rdr = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(s.as_bytes());
            let mut rec = csv::StringRecord::new();
            if !rdr
                .read_record(&mut rec)
                .map_err(|e| HtapError::InvalidArgument(format!("failed to parse CSV: {e}")))?
            {
                return Err(HtapError::InvalidArgument("empty CSV record".into()));
            }
            let header_map = CsvHeaderMap::identity(schema.len());
            decode_csv_record(schema, &header_map, &rec)
        }
        DataFormat::JsonLines => {
            let s = std::str::from_utf8(data)
                .map_err(|e| HtapError::InvalidArgument(format!("non-UTF8 JSON record: {e}")))?;
            decode_json_line(schema, s)
        }
    }
}

/// Generic record encoder dispatching on [`DataFormat`].
pub fn encode_record(schema: &Schema, format: DataFormat, row: &Row) -> Result<Vec<u8>> {
    match format {
        DataFormat::Csv => {
            let rec = encode_csv_record(schema, row)?;
            let mut buf = Vec::new();
            {
                let mut wtr = csv::WriterBuilder::new()
                    .has_headers(false)
                    .from_writer(&mut buf);
                wtr.write_record(&rec).map_err(|e| {
                    HtapError::Internal(format!("failed to format CSV record: {e}"))
                })?;
                wtr.flush()
                    .map_err(|e| HtapError::Internal(format!("failed to flush CSV writer: {e}")))?;
            }
            // Strip trailing newline if generated
            if buf.ends_with(b"\n") {
                buf.pop();
                if buf.ends_with(b"\r") {
                    buf.pop();
                }
            }
            Ok(buf)
        }
        DataFormat::JsonLines => {
            let line = encode_json_line(schema, row)?;
            Ok(line.into_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_encoding_and_decoding() {
        assert_eq!(encode_hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(encode_hex(&[]), "");

        assert_eq!(
            decode_hex("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            decode_hex("0xDEADBEEF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());

        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("xyz1").is_err());
    }

    #[test]
    fn test_csv_field_parsing_and_limits() {
        let bool_col = ColumnDef {
            name: "b".into(),
            data_type: DataType::Bool,
            nullable: true,
            primary_key: false,
        };
        assert_eq!(
            parse_csv_field(&bool_col, "true").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(parse_csv_field(&bool_col, "1").unwrap(), Value::Bool(true));
        assert_eq!(parse_csv_field(&bool_col, "T").unwrap(), Value::Bool(true));
        assert_eq!(
            parse_csv_field(&bool_col, "false").unwrap(),
            Value::Bool(false)
        );
        assert_eq!(parse_csv_field(&bool_col, "0").unwrap(), Value::Bool(false));
        assert_eq!(parse_csv_field(&bool_col, "F").unwrap(), Value::Bool(false));
        assert_eq!(parse_csv_field(&bool_col, r"\N").unwrap(), Value::Null);
        assert!(parse_csv_field(&bool_col, "not_bool").is_err());

        let non_null_bool = ColumnDef {
            name: "b".into(),
            data_type: DataType::Bool,
            nullable: false,
            primary_key: false,
        };
        assert!(parse_csv_field(&non_null_bool, r"\N").is_err());

        let float_col = ColumnDef {
            name: "f".into(),
            data_type: DataType::Float64,
            nullable: true,
            primary_key: false,
        };
        assert_eq!(
            parse_csv_field(&float_col, "3.25").unwrap(),
            Value::Float64(3.25)
        );
        assert!(
            matches!(parse_csv_field(&float_col, "NaN").unwrap(), Value::Float64(v) if v.is_nan())
        );
        assert_eq!(
            parse_csv_field(&float_col, "inf").unwrap(),
            Value::Float64(f64::INFINITY)
        );
    }

    #[test]
    fn test_record_encode_decode_roundtrip() {
        let schema = Schema::new(vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "val".into(),
                data_type: DataType::String,
                nullable: true,
                primary_key: false,
            },
        ])
        .unwrap();

        let row = Row::new(vec![Value::Int32(100), Value::String("hello world".into())]);

        // CSV roundtrip
        let csv_bytes = encode_record(&schema, DataFormat::Csv, &row).unwrap();
        let decoded_csv = decode_record(&schema, DataFormat::Csv, &csv_bytes).unwrap();
        assert_eq!(decoded_csv, row);

        // JSONLines roundtrip
        let json_bytes = encode_record(&schema, DataFormat::JsonLines, &row).unwrap();
        let decoded_json = decode_record(&schema, DataFormat::JsonLines, &json_bytes).unwrap();
        assert_eq!(decoded_json, row);
    }

    #[test]
    fn test_timestamp_calendar_date_and_datetime_imports() {
        let timestamp_col = ColumnDef {
            name: "ts".into(),
            data_type: DataType::Timestamp,
            nullable: false,
            primary_key: false,
        };

        let date_text = "2024-01-02";
        let datetime_text = "2024-01-02 03:04:05";
        let raw_micros = "1704164645000000";

        let expected_date = Value::Timestamp(parse_date_to_timestamp_micros(date_text).unwrap());
        let expected_datetime = Value::Timestamp(
            parse_date_to_timestamp_micros(date_text).unwrap()
                + (3 * 3600 + 4 * 60 + 5) * 1_000_000,
        );
        let expected_raw = Value::Timestamp(raw_micros.parse::<i64>().unwrap());

        // CSV imports support raw microseconds, calendar dates, and space-separated datetimes.
        assert_eq!(
            parse_csv_field(&timestamp_col, raw_micros).unwrap(),
            expected_raw
        );
        assert_eq!(
            parse_csv_field(&timestamp_col, date_text).unwrap(),
            expected_date
        );
        assert_eq!(
            parse_csv_field(&timestamp_col, datetime_text).unwrap(),
            expected_datetime
        );
        assert!(parse_csv_field(&timestamp_col, "2024-02-30").is_err());

        // JSONLines imports support the same timestamp representations.
        assert_eq!(
            parse_json_value(
                &timestamp_col,
                &serde_json::Value::Number(serde_json::Number::from(
                    raw_micros.parse::<i64>().unwrap()
                ))
            )
            .unwrap(),
            expected_raw
        );
        assert_eq!(
            parse_json_value(&timestamp_col, &serde_json::Value::String(date_text.into())).unwrap(),
            expected_date
        );
        assert_eq!(
            parse_json_value(
                &timestamp_col,
                &serde_json::Value::String(datetime_text.into())
            )
            .unwrap(),
            expected_datetime
        );
        assert!(parse_json_value(
            &timestamp_col,
            &serde_json::Value::String("2024-02-30".into())
        )
        .is_err());
    }

    #[test]
    fn test_csv_header_map_and_headerless_decode() {
        let schema = Schema::new(vec![
            ColumnDef {
                name: "id".into(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: true,
            },
            ColumnDef {
                name: "name".into(),
                data_type: DataType::String,
                nullable: false,
                primary_key: false,
            },
            ColumnDef {
                name: "flag".into(),
                data_type: DataType::Bool,
                nullable: true,
                primary_key: false,
            },
        ])
        .unwrap();

        // from_schema produces 1:1 schema declaration order
        let header_map = CsvHeaderMap::from_schema(&schema);
        assert_eq!(header_map.schema_to_csv(0), 0);
        assert_eq!(header_map.schema_to_csv(1), 1);
        assert_eq!(header_map.schema_to_csv(2), 2);

        // Valid record matching schema.len()
        let rec = csv::StringRecord::from(vec!["42", "alice", "true"]);
        let row = decode_csv_record(&schema, &header_map, &rec).unwrap();
        assert_eq!(
            row.values(),
            &[
                Value::Int32(42),
                Value::String("alice".into()),
                Value::Bool(true)
            ]
        );

        // Record with too few fields rejected
        let rec_few = csv::StringRecord::from(vec!["42", "alice"]);
        let err = decode_csv_record(&schema, &header_map, &rec_few).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));

        // Record with too many fields rejected
        let rec_many = csv::StringRecord::from(vec!["42", "alice", "true", "extra"]);
        let err = decode_csv_record(&schema, &header_map, &rec_many).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    #[test]
    fn test_timestamp_datetime_requires_two_digit_time_components() {
        for invalid in [
            "2024-01-02 3:4:5",
            "2024-01-02 +1:02:03",
            "2024-01-02 01:02:003",
        ] {
            assert!(
                parse_timestamp_text(invalid).is_err(),
                "expected invalid datetime '{invalid}' to be rejected"
            );
        }
    }
}
