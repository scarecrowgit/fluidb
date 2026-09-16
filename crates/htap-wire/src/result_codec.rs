//! Encoding of engine results as MySQL text-protocol packets, and the inverse for the client.
//!
//! Three response shapes exist:
//! - a command OK packet (header `0x00`) answering DDL/DML, `COM_PING`, `COM_INIT_DB` and auth;
//! - an ERR packet (header `0xFF`, see [`crate::error_map`]);
//! - a result set: column count, column definitions, optional legacy EOF, text rows, and a
//!   terminator whose header is always `0xFE` (a legacy EOF packet, or an OK-shaped packet when
//!   `CLIENT_DEPRECATE_EOF` was negotiated).
//!
//! Type mapping: `Bool` → `TINY` (`0`/`1`), `Int32` → `LONG`, `Int64` → `LONGLONG`,
//! `Float64` → `DOUBLE`, `String` → `VAR_STRING` (utf8mb4), `Bytes` → `BLOB` (binary, raw
//! bytes, never hex), `Timestamp` → `DATETIME` rendered as `YYYY-MM-DD HH:MM:SS.ffffff` in UTC.

use std::io;

use htap_common::types::{ColumnDef, DataType, Row, Value};

use crate::codec::{
    read_fixed, read_lenenc_int, read_lenenc_str, read_u16, read_u32, write_lenenc_int,
    write_lenenc_str,
};
use crate::proto::*;

/// Wire description of a column type: `(type_code, collation, column_length, decimals)`.
pub fn mysql_type_for(dt: DataType) -> (u8, u16, u32, u8) {
    match dt {
        DataType::Bool => (MYSQL_TYPE_TINY, COLLATION_BINARY, 1, 0),
        DataType::Int32 => (MYSQL_TYPE_LONG, COLLATION_BINARY, 11, 0),
        DataType::Int64 => (MYSQL_TYPE_LONGLONG, COLLATION_BINARY, 20, 0),
        DataType::Float64 => (MYSQL_TYPE_DOUBLE, COLLATION_BINARY, 22, 31),
        DataType::String => (MYSQL_TYPE_VAR_STRING, COLLATION_UTF8MB4, 65_535 * 4, 0),
        DataType::Bytes => (MYSQL_TYPE_BLOB, COLLATION_BINARY, u32::MAX, 0),
        DataType::Timestamp => (MYSQL_TYPE_DATETIME, COLLATION_BINARY, 26, 6),
    }
}

/// Inverse of [`mysql_type_for`] for decoding column definitions on the client.
pub fn data_type_for(type_code: u8, collation: u16) -> io::Result<DataType> {
    Ok(match type_code {
        MYSQL_TYPE_TINY => DataType::Bool,
        MYSQL_TYPE_LONG => DataType::Int32,
        MYSQL_TYPE_LONGLONG => DataType::Int64,
        MYSQL_TYPE_DOUBLE => DataType::Float64,
        MYSQL_TYPE_DATETIME => DataType::Timestamp,
        MYSQL_TYPE_VAR_STRING | 0x0f | 0xfe => DataType::String,
        MYSQL_TYPE_BLOB if collation == COLLATION_BINARY => DataType::Bytes,
        MYSQL_TYPE_BLOB => DataType::String,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported column type 0x{other:02x}"),
            ))
        }
    })
}

/// Builds a `ColumnDefinition41` payload.
pub fn build_column_def41(col: &ColumnDef) -> Vec<u8> {
    let (type_code, collation, length, decimals) = mysql_type_for(col.data_type);
    let mut buf = Vec::with_capacity(48 + col.name.len() * 2);
    write_lenenc_str(&mut buf, b"def");
    write_lenenc_str(&mut buf, DEFAULT_SCHEMA.as_bytes());
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, b"");
    write_lenenc_str(&mut buf, col.name.as_bytes());
    write_lenenc_str(&mut buf, col.name.as_bytes());
    write_lenenc_int(&mut buf, 0x0c);
    buf.extend_from_slice(&collation.to_le_bytes());
    buf.extend_from_slice(&length.to_le_bytes());
    buf.push(type_code);
    let mut flags = 0u16;
    if !col.nullable {
        flags |= NOT_NULL_FLAG;
    }
    if col.primary_key {
        flags |= PRI_KEY_FLAG;
    }
    if col.data_type == DataType::Bytes {
        flags |= BINARY_FLAG;
    }
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.push(decimals);
    buf.extend_from_slice(&[0, 0]);
    buf
}

/// Parses a `ColumnDefinition41` payload (client side).
pub fn parse_column_def41(payload: &[u8]) -> io::Result<ColumnDef> {
    let mut pos = 0;
    let _catalog = read_lenenc_str(payload, &mut pos)?;
    let _schema = read_lenenc_str(payload, &mut pos)?;
    let _table = read_lenenc_str(payload, &mut pos)?;
    let _org_table = read_lenenc_str(payload, &mut pos)?;
    let name = String::from_utf8_lossy(read_lenenc_str(payload, &mut pos)?).into_owned();
    let _org_name = read_lenenc_str(payload, &mut pos)?;
    let _fixed = read_lenenc_int(payload, &mut pos)?;
    let collation = read_u16(payload, &mut pos)?;
    let _length = read_u32(payload, &mut pos)?;
    let type_code = read_fixed(payload, &mut pos, 1)?[0];
    let flags = read_u16(payload, &mut pos)?;
    let data_type = data_type_for(type_code, collation)?;
    Ok(ColumnDef {
        name,
        data_type,
        nullable: flags & NOT_NULL_FLAG == 0,
        primary_key: flags & PRI_KEY_FLAG != 0,
    })
}

/// Builds a command OK packet (header `0x00`).
///
/// The status flags are always `SERVER_STATUS_AUTOCOMMIT`; `SERVER_STATUS_IN_TRANS` is never
/// set because the engine has no multi-statement transactions.
pub fn build_command_ok(affected: u64, info: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + info.len());
    buf.push(OK_HEADER);
    write_lenenc_int(&mut buf, affected);
    write_lenenc_int(&mut buf, 0);
    buf.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    // The manual documents `info` as `string<EOF>`, but MySQL servers actually send it
    // length-encoded and drivers parse it that way; do the same.
    if !info.is_empty() {
        write_lenenc_str(&mut buf, info.as_bytes());
    }
    buf
}

/// Builds the packet that terminates a result set (and, in legacy mode, the column block).
///
/// The header is always `0xFE`. When `deprecate_eof` is negotiated the body is OK-shaped and
/// kept under 9 bytes in total; otherwise it is a legacy EOF body.
pub fn build_resultset_terminator(deprecate_eof: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.push(EOF_HEADER);
    if deprecate_eof {
        write_lenenc_int(&mut buf, 0);
        write_lenenc_int(&mut buf, 0);
        buf.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
    } else {
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
    }
    buf
}

/// Decoded OK packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OkPacket {
    /// Affected rows.
    pub affected_rows: u64,
    /// Last insert id (always 0 for this server).
    pub last_insert_id: u64,
    /// Status flags.
    pub status: u16,
    /// Warning count.
    pub warnings: u16,
    /// Info string.
    pub info: String,
}

/// Parses an OK packet body (client side). Accepts both `0x00` and `0xFE` headers.
pub fn parse_ok_payload(payload: &[u8]) -> io::Result<OkPacket> {
    let mut pos = 0;
    let header = read_fixed(payload, &mut pos, 1)?[0];
    if header != OK_HEADER && header != EOF_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an OK packet",
        ));
    }
    let affected_rows = read_lenenc_int(payload, &mut pos)?;
    let last_insert_id = read_lenenc_int(payload, &mut pos)?;
    let status = read_u16(payload, &mut pos)?;
    let warnings = read_u16(payload, &mut pos)?;
    let rest = &payload[pos..];
    let info = match read_lenenc_str(rest, &mut 0) {
        Ok(s) => String::from_utf8_lossy(s).into_owned(),
        // Tolerate servers that send `string<EOF>`.
        Err(_) => String::from_utf8_lossy(rest).into_owned(),
    };
    Ok(OkPacket {
        affected_rows,
        last_insert_id,
        status,
        warnings,
        info,
    })
}

/// Returns `true` if `payload` is a result-set terminator (legacy EOF or deprecated-EOF OK).
pub fn is_resultset_terminator(payload: &[u8]) -> bool {
    payload.first() == Some(&EOF_HEADER) && payload.len() < 9
}

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
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

/// Renders microseconds since the Unix epoch as `YYYY-MM-DD HH:MM:SS.ffffff` (UTC).
pub fn micros_to_datetime_string(micros: i64) -> String {
    let secs = micros.div_euclid(MICROS_PER_SEC);
    let frac = micros.rem_euclid(MICROS_PER_SEC);
    let days = secs.div_euclid(SECS_PER_DAY);
    let sod = secs.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{frac:06}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Parses `YYYY-MM-DD HH:MM:SS[.ffffff]` (UTC) into microseconds since the Unix epoch.
pub fn datetime_string_to_micros(s: &str) -> io::Result<i64> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid datetime text: {s:?}"),
        )
    };
    let (date, time) = s.trim().split_once(' ').ok_or_else(bad)?;
    let mut dp = date.split('-');
    let y: i64 = dp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let m: u32 = dp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let d: u32 = dp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (time, None),
    };
    let mut tp = hms.split(':');
    let hh: i64 = tp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let mm: i64 = tp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let ss: i64 = tp.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if tp.next().is_some()
        || !(0..24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..60).contains(&ss)
    {
        return Err(bad());
    }
    let micros_frac: i64 = match frac {
        Some(f) => {
            if f.is_empty() || f.len() > 6 || !f.chars().all(|c| c.is_ascii_digit()) {
                return Err(bad());
            }
            let mut padded = f.to_string();
            while padded.len() < 6 {
                padded.push('0');
            }
            padded.parse().map_err(|_| bad())?
        }
        None => 0,
    };
    let days = days_from_civil(y, m, d);
    let secs = days
        .checked_mul(SECS_PER_DAY)
        .and_then(|v| v.checked_add(hh * 3600 + mm * 60 + ss))
        .ok_or_else(bad)?;
    secs.checked_mul(MICROS_PER_SEC)
        .and_then(|v| v.checked_add(micros_frac))
        .ok_or_else(bad)
}

/// Encodes a row in the text protocol.
pub fn encode_text_row(row: &Row) -> Vec<u8> {
    let mut buf = Vec::with_capacity(row.len() * 8);
    for value in row.values() {
        match value {
            Value::Null => buf.push(NULL_MARKER),
            Value::Bool(b) => write_lenenc_str(&mut buf, if *b { b"1" } else { b"0" }),
            Value::Bytes(bytes) => write_lenenc_str(&mut buf, bytes),
            Value::Timestamp(micros) => {
                write_lenenc_str(&mut buf, micros_to_datetime_string(*micros).as_bytes())
            }
            other => write_lenenc_str(&mut buf, other.to_string().as_bytes()),
        }
    }
    buf
}

fn parse_text_value(bytes: &[u8], dt: DataType) -> io::Result<Value> {
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid {what} text value: {:?}",
                String::from_utf8_lossy(bytes)
            ),
        )
    };
    let text = || std::str::from_utf8(bytes).map_err(|_| bad("utf-8"));
    Ok(match dt {
        DataType::Bool => match text()? {
            "1" | "true" | "TRUE" => Value::Bool(true),
            "0" | "false" | "FALSE" => Value::Bool(false),
            _ => return Err(bad("bool")),
        },
        DataType::Int32 => Value::Int32(text()?.parse().map_err(|_| bad("int"))?),
        DataType::Int64 => Value::Int64(text()?.parse().map_err(|_| bad("bigint"))?),
        DataType::Float64 => Value::Float64(text()?.parse().map_err(|_| bad("double"))?),
        DataType::String => Value::String(text()?.to_string()),
        DataType::Bytes => Value::Bytes(bytes.to_vec()),
        DataType::Timestamp => Value::Timestamp(datetime_string_to_micros(text()?)?),
    })
}

/// Decodes a text-protocol row using the column types (client side).
pub fn decode_text_row(payload: &[u8], columns: &[ColumnDef]) -> io::Result<Row> {
    let mut pos = 0;
    let mut values = Vec::with_capacity(columns.len());
    for col in columns {
        if payload.get(pos) == Some(&NULL_MARKER) {
            pos += 1;
            values.push(Value::Null);
            continue;
        }
        let bytes = read_lenenc_str(payload, &mut pos)?;
        values.push(parse_text_value(bytes, col.data_type)?);
    }
    if pos != payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes in text row",
        ));
    }
    Ok(Row::new(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, dt: DataType, nullable: bool, pk: bool) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: dt,
            nullable,
            primary_key: pk,
        }
    }

    #[test]
    fn column_def_round_trip_all_types() {
        let types = [
            DataType::Bool,
            DataType::Int32,
            DataType::Int64,
            DataType::Float64,
            DataType::String,
            DataType::Bytes,
            DataType::Timestamp,
        ];
        for (i, dt) in types.iter().enumerate() {
            let c = col(&format!("c{i}"), *dt, i % 2 == 0, i == 1);
            let decoded = parse_column_def41(&build_column_def41(&c)).unwrap();
            assert_eq!(decoded, c);
        }
        assert!(data_type_for(0x10, 63).is_err());
    }

    #[test]
    fn text_row_null_and_bytes_are_raw_not_hex() {
        let columns = vec![
            col("b", DataType::Bytes, true, false),
            col("n", DataType::Int64, true, false),
            col("f", DataType::Bool, false, false),
        ];
        let row = Row::new(vec![
            Value::Bytes(vec![0x00, 0xff, 0x10]),
            Value::Null,
            Value::Bool(true),
        ]);
        let payload = encode_text_row(&row);
        // lenenc(3) + raw bytes, then NULL marker, then "1".
        assert_eq!(payload, [3, 0x00, 0xff, 0x10, NULL_MARKER, 1, b'1']);
        assert_eq!(decode_text_row(&payload, &columns).unwrap(), row);
    }

    #[test]
    fn text_row_numeric_and_timestamp_round_trip() {
        let columns = vec![
            col("i", DataType::Int32, false, false),
            col("l", DataType::Int64, false, false),
            col("d", DataType::Float64, false, false),
            col("s", DataType::String, false, false),
            col("t", DataType::Timestamp, false, false),
        ];
        let row = Row::new(vec![
            Value::Int32(-7),
            Value::Int64(i64::MAX),
            Value::Float64(-1.5e300),
            Value::String("héllo, wörld".into()),
            Value::Timestamp(1_700_000_000_123_456),
        ]);
        let payload = encode_text_row(&row);
        assert_eq!(decode_text_row(&payload, &columns).unwrap(), row);
        assert!(decode_text_row(&payload[..5], &columns).is_err());
    }

    #[test]
    fn datetime_text_conversions() {
        assert_eq!(micros_to_datetime_string(0), "1970-01-01 00:00:00.000000");
        assert_eq!(
            micros_to_datetime_string(1_700_000_000_123_456),
            "2023-11-14 22:13:20.123456"
        );
        assert_eq!(micros_to_datetime_string(-1), "1969-12-31 23:59:59.999999");
        for micros in [
            0i64,
            -1,
            1,
            951_782_400_000_000,     // 2000-02-29
            -2_208_988_800_000_000,  // 1900-01-01
            253_402_300_799_999_999, // 9999-12-31 23:59:59.999999
        ] {
            let text = micros_to_datetime_string(micros);
            assert_eq!(datetime_string_to_micros(&text).unwrap(), micros, "{text}");
        }
        assert_eq!(
            datetime_string_to_micros("2023-11-14 22:13:20").unwrap(),
            1_700_000_000_000_000
        );
        assert_eq!(
            datetime_string_to_micros("2023-11-14 22:13:20.5").unwrap(),
            1_700_000_000_500_000
        );
        assert!(datetime_string_to_micros("2023-13-01 00:00:00").is_err());
        assert!(datetime_string_to_micros("garbage").is_err());
    }

    #[test]
    fn command_ok_header_is_always_0x00_and_never_in_trans() {
        let ok = build_command_ok(3, "version=9");
        assert_eq!(ok[0], OK_HEADER);
        let parsed = parse_ok_payload(&ok).unwrap();
        assert_eq!(parsed.affected_rows, 3);
        assert_eq!(
            parsed.status & 0x0001,
            0,
            "SERVER_STATUS_IN_TRANS must never be set"
        );
        assert_eq!(parsed.status, SERVER_STATUS_AUTOCOMMIT);
        assert_eq!(parsed.info, "version=9");
        assert_eq!(ok[7], 9, "info must be length-encoded");
        assert_eq!(parse_ok_payload(&build_command_ok(0, "")).unwrap().info, "");
        assert!(parse_ok_payload(&[0xff, 0, 0]).is_err());
    }

    #[test]
    fn terminator_header_is_always_0xfe_legacy_and_deprecated() {
        let legacy = build_resultset_terminator(false);
        assert_eq!(legacy, [EOF_HEADER, 0, 0, 2, 0]);
        assert!(is_resultset_terminator(&legacy));
        let modern = build_resultset_terminator(true);
        assert_eq!(modern[0], EOF_HEADER);
        assert!(modern.len() < 9);
        assert!(is_resultset_terminator(&modern));
        let parsed = parse_ok_payload(&modern).unwrap();
        assert_eq!(parsed.status, SERVER_STATUS_AUTOCOMMIT);
        assert!(!is_resultset_terminator(&build_command_ok(0, "")));
    }
}
