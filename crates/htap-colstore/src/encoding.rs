//! Encoding and compression routines for columnar storage.
//!
//! Provides null bitmap packing, fixed-width little-endian plain encodings,
//! per-block dictionary encodings for string and byte data, zstd compression,
//! and typed value serialization for zone map metadata.

use std::collections::HashMap;

use htap_common::{DataType, HtapError, Result, Value};

use crate::types::{
    MAX_BLOCK_ROWS, MAX_BLOCK_STORED_BYTES, MAX_BLOCK_UNCOMPRESSED_BYTES, MAX_VALUE_BYTES,
};

/// Encodes a validity slice into a compact null bitmap (1 bit per row, `1 = non-null`).
#[must_use]
pub fn encode_null_bitmap(validity: &[bool]) -> Vec<u8> {
    let num_bytes = validity.len().div_ceil(8);
    let mut bytes = vec![0u8; num_bytes];
    for (i, &is_valid) in validity.iter().enumerate() {
        if is_valid {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }
    bytes
}

/// Decodes a compact null bitmap into a boolean validity vector.
///
/// # Errors
/// Returns [`HtapError::Corruption`] if:
/// - The byte slice length does not match `div_ceil(row_count, 8)`.
/// - Unused padding bits in the final byte are not zero.
pub fn decode_null_bitmap(bytes: &[u8], row_count: usize) -> Result<Vec<bool>> {
    let expected_bytes = row_count.div_ceil(8);
    if bytes.len() != expected_bytes {
        return Err(HtapError::Corruption(format!(
            "null bitmap length {} does not match expected {}",
            bytes.len(),
            expected_bytes
        )));
    }

    if !row_count.is_multiple_of(8) && !bytes.is_empty() {
        let unused_mask = !((1u8 << (row_count % 8)) - 1);
        if (bytes[expected_bytes - 1] & unused_mask) != 0 {
            return Err(HtapError::Corruption(
                "null bitmap contains non-zero unused padding bits".into(),
            ));
        }
    }

    let mut validity = Vec::with_capacity(row_count);
    for i in 0..row_count {
        validity.push((bytes[i / 8] & (1 << (i % 8))) != 0);
    }
    Ok(validity)
}

/// Encodes non-null values using fixed-width little-endian or length-prefixed plain encoding.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if any value does not match `data_type` or exceeds limits.
pub fn encode_plain(data_type: DataType, non_null_values: &[Value]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    for val in non_null_values {
        match (data_type, val) {
            (DataType::Bool, Value::Bool(b)) => {
                buf.push(if *b { 1 } else { 0 });
            }
            (DataType::Int32, Value::Int32(i)) => {
                buf.extend_from_slice(&i.to_le_bytes());
            }
            (DataType::Int64, Value::Int64(i)) => {
                buf.extend_from_slice(&i.to_le_bytes());
            }
            (DataType::Timestamp, Value::Timestamp(t)) => {
                buf.extend_from_slice(&t.to_le_bytes());
            }
            (DataType::Float64, Value::Float64(f)) => {
                buf.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            (DataType::String, Value::String(s)) => {
                if s.len() > MAX_VALUE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "string value length {} exceeds MAX_VALUE_BYTES {}",
                        s.len(),
                        MAX_VALUE_BYTES
                    )));
                }
                buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            (DataType::Bytes, Value::Bytes(b)) => {
                if b.len() > MAX_VALUE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "bytes value length {} exceeds MAX_VALUE_BYTES {}",
                        b.len(),
                        MAX_VALUE_BYTES
                    )));
                }
                buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                buf.extend_from_slice(b);
            }
            (
                DataType::Decimal { precision, scale },
                Value::Decimal {
                    value,
                    precision: value_precision,
                    scale: value_scale,
                },
            ) if precision == *value_precision && scale == *value_scale => {
                buf.extend_from_slice(&value.to_le_bytes());
            }
            _ => {
                return Err(HtapError::InvalidArgument(format!(
                    "value {val:?} does not match expected column type {data_type}"
                )));
            }
        }
    }
    Ok(buf)
}

/// Decodes non-null values from a plain-encoded byte buffer.
///
/// # Errors
/// Returns [`HtapError::Corruption`] if the byte buffer is truncated, has trailing bytes,
/// or contains invalid boolean or UTF-8 values.
pub fn decode_plain(data_type: DataType, bytes: &[u8], count: usize) -> Result<Vec<Value>> {
    use htap_common::bytecursor::ByteReader;

    let mut reader = ByteReader::new(bytes);
    let mut values = Vec::with_capacity(count);

    for _ in 0..count {
        match data_type {
            DataType::Bool => {
                let b = reader.read_u8().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain bool".into())
                })?;
                match b {
                    0 => values.push(Value::Bool(false)),
                    1 => values.push(Value::Bool(true)),
                    _ => {
                        return Err(HtapError::Corruption(format!(
                            "invalid boolean byte {b} in plain encoding"
                        )))
                    }
                }
            }
            DataType::Int32 => {
                let v = reader.read_i32_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain int32".into())
                })?;
                values.push(Value::Int32(v));
            }
            DataType::Int64 => {
                let v = reader.read_i64_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain int64".into())
                })?;
                values.push(Value::Int64(v));
            }
            DataType::Timestamp => {
                let v = reader.read_i64_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain timestamp".into())
                })?;
                values.push(Value::Timestamp(v));
            }
            DataType::Float64 => {
                let v = reader.read_f64_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain float64".into())
                })?;
                values.push(Value::Float64(v));
            }
            DataType::String => {
                let len = reader.read_u32_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain string length".into())
                })? as usize;
                if len > MAX_VALUE_BYTES {
                    return Err(HtapError::Corruption(format!(
                        "invalid plain string byte length {len}"
                    )));
                }
                let string_bytes = reader.read_bytes(len).map_err(|_| {
                    HtapError::Corruption(format!("invalid plain string byte length {len}"))
                })?;
                let s = std::str::from_utf8(string_bytes).map_err(|e| {
                    HtapError::Corruption(format!("invalid UTF-8 in plain string: {e}"))
                })?;
                values.push(Value::String(s.to_string()));
            }
            DataType::Bytes => {
                let len = reader.read_u32_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain bytes length".into())
                })? as usize;
                if len > MAX_VALUE_BYTES {
                    return Err(HtapError::Corruption(format!(
                        "invalid plain bytes length {len}"
                    )));
                }
                let b = reader
                    .read_bytes(len)
                    .map_err(|_| {
                        HtapError::Corruption(format!("invalid plain bytes length {len}"))
                    })?
                    .to_vec();
                values.push(Value::Bytes(b));
            }
            DataType::Decimal { precision, scale } => {
                let value = reader.read_i64_le().map_err(|_| {
                    HtapError::Corruption("unexpected EOF decoding plain decimal".into())
                })?;
                values.push(Value::Decimal {
                    value,
                    precision,
                    scale,
                });
            }
        }
    }

    reader.expect_exhausted().map_err(|_| {
        HtapError::Corruption(format!(
            "trailing bytes in plain block payload: consumed {}, total {}",
            reader.position(),
            bytes.len()
        ))
    })?;

    Ok(values)
}

/// Encodes non-null string or bytes values using dictionary encoding.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if `data_type` is not `String` or `Bytes`.
pub fn encode_dictionary(data_type: DataType, non_null_values: &[Value]) -> Result<Vec<u8>> {
    if data_type != DataType::String && data_type != DataType::Bytes {
        return Err(HtapError::InvalidArgument(format!(
            "dictionary encoding only supported for String and Bytes, got {data_type}"
        )));
    }

    let mut dict_entries: Vec<Vec<u8>> = Vec::new();
    let mut code_map: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut codes: Vec<u32> = Vec::with_capacity(non_null_values.len());

    for val in non_null_values {
        let slice: &[u8] = match val {
            Value::String(s) => {
                if s.len() > MAX_VALUE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "string length {} exceeds MAX_VALUE_BYTES",
                        s.len()
                    )));
                }
                s.as_bytes()
            }
            Value::Bytes(b) => {
                if b.len() > MAX_VALUE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "bytes length {} exceeds MAX_VALUE_BYTES",
                        b.len()
                    )));
                }
                b.as_slice()
            }
            _ => {
                return Err(HtapError::InvalidArgument(format!(
                    "expected {data_type}, got {val:?}"
                )));
            }
        };

        if let Some(&code) = code_map.get(slice) {
            codes.push(code);
        } else {
            let code = dict_entries.len() as u32;
            dict_entries.push(slice.to_vec());
            code_map.insert(slice.to_vec(), code);
            codes.push(code);
        }
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(&(dict_entries.len() as u32).to_le_bytes());
    for entry in &dict_entries {
        buf.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        buf.extend_from_slice(entry);
    }
    buf.extend_from_slice(&(codes.len() as u32).to_le_bytes());
    for code in &codes {
        buf.extend_from_slice(&code.to_le_bytes());
    }

    Ok(buf)
}

/// Decodes non-null values from a dictionary-encoded byte buffer.
///
/// # Errors
/// Returns [`HtapError::Corruption`] if dictionary structure or codes are malformed.
pub fn decode_dictionary(data_type: DataType, bytes: &[u8], count: usize) -> Result<Vec<Value>> {
    use htap_common::bytecursor::ByteReader;

    if data_type != DataType::String && data_type != DataType::Bytes {
        return Err(HtapError::Corruption(format!(
            "dictionary encoding only valid for String and Bytes, got {data_type}"
        )));
    }

    let mut reader = ByteReader::new(bytes);
    let dict_entry_count = reader.read_u32_le().map_err(|_| {
        HtapError::Corruption("unexpected EOF reading dictionary entry count".into())
    })? as usize;

    if dict_entry_count > MAX_BLOCK_ROWS {
        return Err(HtapError::Corruption(format!(
            "dictionary entry count {dict_entry_count} exceeds MAX_BLOCK_ROWS {MAX_BLOCK_ROWS}"
        )));
    }

    let mut dict = Vec::with_capacity(dict_entry_count);
    for _ in 0..dict_entry_count {
        let entry_len = reader.read_u32_le().map_err(|_| {
            HtapError::Corruption("unexpected EOF reading dictionary entry length".into())
        })? as usize;
        if entry_len > MAX_VALUE_BYTES {
            return Err(HtapError::Corruption(format!(
                "invalid dictionary entry length {entry_len}"
            )));
        }
        let entry_slice = reader.read_bytes(entry_len).map_err(|_| {
            HtapError::Corruption(format!("invalid dictionary entry length {entry_len}"))
        })?;

        if data_type == DataType::String {
            let s = std::str::from_utf8(entry_slice).map_err(|e| {
                HtapError::Corruption(format!("invalid UTF-8 in dictionary entry: {e}"))
            })?;
            dict.push(Value::String(s.to_string()));
        } else {
            dict.push(Value::Bytes(entry_slice.to_vec()));
        }
    }

    let value_count = reader.read_u32_le().map_err(|_| {
        HtapError::Corruption("unexpected EOF reading dictionary value count".into())
    })? as usize;

    if value_count != count {
        return Err(HtapError::Corruption(format!(
            "dictionary value count {value_count} does not match expected non-null count {count}"
        )));
    }

    let mut values = Vec::with_capacity(value_count);
    for _ in 0..value_count {
        let code = reader
            .read_u32_le()
            .map_err(|_| HtapError::Corruption("unexpected EOF reading dictionary code".into()))?
            as usize;
        if code >= dict.len() {
            return Err(HtapError::Corruption(format!(
                "dictionary code {code} out of bounds for dictionary of size {}",
                dict.len()
            )));
        }
        values.push(dict[code].clone());
    }

    reader.expect_exhausted().map_err(|_| {
        HtapError::Corruption(format!(
            "trailing bytes in dictionary payload: consumed {}, total {}",
            reader.position(),
            bytes.len()
        ))
    })?;

    Ok(values)
}

/// Compresses a raw payload with zstd, only using compressed output if at least 10% smaller.
///
/// Returns `(stored_bytes, is_compressed)`.
///
/// # Errors
/// Returns [`HtapError::Internal`] if zstd compression fails.
pub fn compress_payload(raw: &[u8], zstd_level: i32) -> Result<(Vec<u8>, bool)> {
    if raw.is_empty() {
        return Ok((Vec::new(), false));
    }

    let compressed = zstd::encode_all(raw, zstd_level)
        .map_err(|e| HtapError::Internal(format!("zstd compression failed: {e}")))?;

    // Only compress if at least 10% smaller: compressed.len() * 10 <= raw.len() * 9
    if compressed.len() * 10 <= raw.len() * 9 {
        Ok((compressed, true))
    } else {
        Ok((raw.to_vec(), false))
    }
}

/// Decompresses a stored payload buffer using zstd if compressed, or returns raw bytes.
///
/// # Errors
/// Returns [`HtapError::Corruption`] if sizes exceed limits or decompression fails.
pub fn decompress_payload(stored: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    if stored.len() > MAX_BLOCK_STORED_BYTES {
        return Err(HtapError::Corruption(format!(
            "stored payload length {} exceeds MAX_BLOCK_STORED_BYTES {MAX_BLOCK_STORED_BYTES}",
            stored.len()
        )));
    }
    if raw_len > MAX_BLOCK_UNCOMPRESSED_BYTES {
        return Err(HtapError::Corruption(format!(
            "raw payload length {raw_len} exceeds MAX_BLOCK_UNCOMPRESSED_BYTES {MAX_BLOCK_UNCOMPRESSED_BYTES}"
        )));
    }
    if stored.len() > raw_len {
        return Err(HtapError::Corruption(format!(
            "stored payload length {} exceeds raw length {raw_len}",
            stored.len()
        )));
    }

    if stored.len() == raw_len {
        return Ok(stored.to_vec());
    }

    let decompressed = zstd::decode_all(stored)
        .map_err(|e| HtapError::Corruption(format!("zstd decompression failed: {e}")))?;

    if decompressed.len() != raw_len {
        return Err(HtapError::Corruption(format!(
            "decompressed payload length mismatch: expected {raw_len}, got {}",
            decompressed.len()
        )));
    }

    Ok(decompressed)
}

/// Serializes a non-null typed [`Value`] into a byte buffer for zone map min/max persistence.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if `val` is NULL or exceeds [`MAX_VALUE_BYTES`].
pub fn encode_typed_value(val: &Value, buf: &mut Vec<u8>) -> Result<()> {
    match val {
        Value::Null => Err(HtapError::InvalidArgument(
            "cannot encode NULL as typed zone map value".into(),
        )),
        Value::Bool(b) => {
            buf.push(if *b { 1 } else { 0 });
            Ok(())
        }
        Value::Int32(i) => {
            buf.extend_from_slice(&i.to_le_bytes());
            Ok(())
        }
        Value::Int64(i) => {
            buf.extend_from_slice(&i.to_le_bytes());
            Ok(())
        }
        Value::Timestamp(t) => {
            buf.extend_from_slice(&t.to_le_bytes());
            Ok(())
        }
        Value::Float64(f) => {
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
            Ok(())
        }
        Value::String(s) => {
            if s.len() > MAX_VALUE_BYTES {
                return Err(HtapError::InvalidArgument(format!(
                    "string length {} exceeds MAX_VALUE_BYTES",
                    s.len()
                )));
            }
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
            Ok(())
        }
        Value::Bytes(b) => {
            if b.len() > MAX_VALUE_BYTES {
                return Err(HtapError::InvalidArgument(format!(
                    "bytes length {} exceeds MAX_VALUE_BYTES",
                    b.len()
                )));
            }
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
            Ok(())
        }
        Value::Decimal { value, .. } => {
            buf.extend_from_slice(&value.to_le_bytes());
            Ok(())
        }
    }
}

/// Deserializes a non-null typed [`Value`] from a byte buffer at the given cursor position.
///
/// # Errors
/// Returns [`HtapError::Corruption`] if buffer is truncated or data is invalid.
pub fn decode_typed_value(dt: DataType, bytes: &[u8], cursor: &mut usize) -> Result<Value> {
    use htap_common::bytecursor::ByteReader;

    macro_rules! reader_from_cursor {
        ($message:literal) => {{
            let remaining = bytes
                .get(*cursor..)
                .ok_or_else(|| HtapError::Corruption($message.into()))?;
            ByteReader::new(remaining)
        }};
    }

    match dt {
        DataType::Bool => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map bool");
            let byte = reader.read_u8().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map bool".into())
            })?;
            *cursor += reader.position();
            match byte {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                _ => Err(HtapError::Corruption(
                    "invalid bool byte in zone map".into(),
                )),
            }
        }
        DataType::Int32 => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map int32");
            let v = reader.read_i32_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map int32".into())
            })?;
            *cursor += reader.position();
            Ok(Value::Int32(v))
        }
        DataType::Int64 => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map int64");
            let v = reader.read_i64_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map int64".into())
            })?;
            *cursor += reader.position();
            Ok(Value::Int64(v))
        }
        DataType::Timestamp => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map timestamp");
            let v = reader.read_i64_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map timestamp".into())
            })?;
            *cursor += reader.position();
            Ok(Value::Timestamp(v))
        }
        DataType::Float64 => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map float64");
            let v = reader.read_f64_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map float64".into())
            })?;
            *cursor += reader.position();
            Ok(Value::Float64(v))
        }
        DataType::String => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map string len");
            let len = reader.read_u32_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map string len".into())
            })? as usize;
            *cursor += reader.position();

            if len > MAX_VALUE_BYTES {
                return Err(HtapError::Corruption(
                    "invalid string len in zone map".into(),
                ));
            }

            let string_bytes = reader
                .read_bytes(len)
                .map_err(|_| HtapError::Corruption("invalid string len in zone map".into()))?;
            let s = std::str::from_utf8(string_bytes).map_err(|e| {
                HtapError::Corruption(format!("invalid UTF-8 in zone map string: {e}"))
            })?;
            *cursor += len;
            Ok(Value::String(s.to_string()))
        }
        DataType::Bytes => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map bytes len");
            let len = reader.read_u32_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map bytes len".into())
            })? as usize;
            *cursor += reader.position();

            if len > MAX_VALUE_BYTES {
                return Err(HtapError::Corruption(
                    "invalid bytes len in zone map".into(),
                ));
            }

            let b = reader
                .read_bytes(len)
                .map_err(|_| HtapError::Corruption("invalid bytes len in zone map".into()))?
                .to_vec();
            *cursor += len;
            Ok(Value::Bytes(b))
        }
        DataType::Decimal { precision, scale } => {
            let mut reader = reader_from_cursor!("unexpected EOF decoding zone map decimal");
            let value = reader.read_i64_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF decoding zone map decimal".into())
            })?;
            *cursor += reader.position();
            Ok(Value::Decimal {
                value,
                precision,
                scale,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_bitmap_roundtrip_and_padding_validation() {
        let validity = vec![
            true, false, true, true, false, false, true, false, true, true,
        ];
        let bytes = encode_null_bitmap(&validity);
        assert_eq!(bytes.len(), 2);
        let decoded = decode_null_bitmap(&bytes, validity.len()).unwrap();
        assert_eq!(validity, decoded);

        // Test non-zero padding bits corruption
        let mut corrupt_bytes = bytes.clone();
        corrupt_bytes[1] |= 0b1000_0000; // 10 rows means byte 1 has only bits 0 and 1 active
        assert!(matches!(
            decode_null_bitmap(&corrupt_bytes, 10),
            Err(HtapError::Corruption(_))
        ));
    }

    #[test]
    fn test_plain_roundtrip_all_types() {
        let cases: Vec<(DataType, Vec<Value>)> = vec![
            (
                DataType::Bool,
                vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)],
            ),
            (
                DataType::Int32,
                vec![Value::Int32(0), Value::Int32(-42), Value::Int32(100)],
            ),
            (
                DataType::Int64,
                vec![
                    Value::Int64(i64::MIN),
                    Value::Int64(0),
                    Value::Int64(i64::MAX),
                ],
            ),
            (
                DataType::Timestamp,
                vec![Value::Timestamp(0), Value::Timestamp(1_700_000_000)],
            ),
            (
                DataType::Float64,
                vec![
                    Value::Float64(0.0),
                    Value::Float64(-0.0),
                    Value::Float64(f64::NAN),
                    Value::Float64(f64::INFINITY),
                    Value::Float64(-1.25),
                ],
            ),
            (
                DataType::String,
                vec![
                    Value::String("".into()),
                    Value::String("hello\0world".into()),
                    Value::String("plain test".into()),
                ],
            ),
            (
                DataType::Bytes,
                vec![
                    Value::Bytes(vec![]),
                    Value::Bytes(vec![0x00, 0xff, 0xaa]),
                    Value::Bytes(b"raw bytes".to_vec()),
                ],
            ),
        ];

        for (dt, vals) in cases {
            let encoded = encode_plain(dt, &vals).unwrap();
            let decoded = decode_plain(dt, &encoded, vals.len()).unwrap();
            assert_eq!(vals.len(), decoded.len());
            for (orig, dec) in vals.iter().zip(decoded.iter()) {
                if let (Value::Float64(f1), Value::Float64(f2)) = (orig, dec) {
                    assert_eq!(f1.to_bits(), f2.to_bits());
                } else {
                    assert_eq!(orig, dec);
                }
            }
        }
    }

    #[test]
    fn test_dictionary_roundtrip() {
        let vals = vec![
            Value::String("alpha".into()),
            Value::String("beta".into()),
            Value::String("alpha".into()),
            Value::String("gamma".into()),
            Value::String("beta".into()),
        ];
        let encoded = encode_dictionary(DataType::String, &vals).unwrap();
        let decoded = decode_dictionary(DataType::String, &encoded, vals.len()).unwrap();
        assert_eq!(vals, decoded);

        // Code out of bounds corruption test
        let mut corrupt_encoded = encoded.clone();
        let last_idx = corrupt_encoded.len() - 1;
        corrupt_encoded[last_idx] = 99; // corrupt last code to out of bounds
        assert!(matches!(
            decode_dictionary(DataType::String, &corrupt_encoded, vals.len()),
            Err(HtapError::Corruption(_))
        ));
    }

    #[test]
    fn test_compression_threshold_and_decompression() {
        // High entropy: will not compress by >= 10%
        let uncompressible: Vec<u8> = (0..255).collect();
        let (stored, is_comp) = compress_payload(&uncompressible, 3).unwrap();
        assert!(!is_comp);
        assert_eq!(stored, uncompressible);

        // Low entropy: will compress easily
        let compressible = vec![b'A'; 1000];
        let (stored_comp, is_comp2) = compress_payload(&compressible, 3).unwrap();
        assert!(is_comp2);
        assert!(stored_comp.len() < compressible.len() / 2);

        let decompressed = decompress_payload(&stored_comp, compressible.len()).unwrap();
        assert_eq!(decompressed, compressible);
    }
}
