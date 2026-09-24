//! Integration tests for columnar segment roundtrip and corruption detection.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use htap_colstore::{
    ColumnEncoding, ColumnVector, SegmentMetadata, SegmentOptions, SegmentReader, SegmentWriter,
    FRAME_HEADER_LEN,
};
use htap_common::{ColumnDef, DataType, HtapError, Row, Schema, Value};
use tempfile::tempdir;

fn test_col(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable,
        primary_key: false,
    }
}

fn full_schema() -> Schema {
    Schema::new(vec![
        test_col("c_bool", DataType::Bool, true),
        test_col("c_int32", DataType::Int32, false),
        test_col("c_int64", DataType::Int64, true),
        test_col("c_float64", DataType::Float64, false),
        test_col("c_string", DataType::String, true),
        test_col("c_bytes", DataType::Bytes, true),
        test_col("c_timestamp", DataType::Timestamp, false),
        test_col(
            "c_decimal",
            DataType::Decimal {
                precision: 18,
                scale: 4,
            },
            false,
        ),
    ])
    .unwrap()
}

fn flip_byte_at(path: &Path, offset: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn test_roundtrip_all_eight_types_and_forced_small_blocks() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("eight_types.col");
    let schema = full_schema();

    // 25 rows with rows_per_block = 7 => 4 blocks (7, 7, 7, 4)
    let opts = SegmentOptions::new().with_rows_per_block(7);

    let mut rows = Vec::new();
    for i in 0..25 {
        let is_even = i % 2 == 0;
        let row = Row::new(vec![
            if is_even {
                Value::Bool(i % 4 == 0)
            } else {
                Value::Null
            },
            Value::Int32(i * 10 - 100),
            if is_even {
                Value::Int64((i as i64) * 1_000_000)
            } else {
                Value::Null
            },
            Value::Float64((i as f64) * 1.5 - 10.0),
            if is_even {
                Value::String(format!("str_{i}"))
            } else {
                Value::Null
            },
            if is_even {
                Value::Bytes(vec![i as u8, (i + 1) as u8])
            } else {
                Value::Null
            },
            Value::Timestamp(1_700_000_000 + (i as i64) * 60),
            Value::Decimal {
                value: match i % 4 {
                    0 => 0,
                    1 => -12_345,
                    2 => (i as i64) * 10_000,
                    _ => -((i as i64) * 1_111),
                },
                precision: 18,
                scale: 4,
            },
        ]);
        rows.push(row);
    }

    let meta: SegmentMetadata = SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    assert_eq!(meta.row_count, 25);
    assert_eq!(meta.column_count, 8);
    assert_eq!(meta.block_count, 4);

    let mut reader = SegmentReader::open(&path).unwrap();
    assert_eq!(reader.metadata(), &meta);
    assert_eq!(reader.block_count(), 4);

    // Read and verify each block
    let mut current_row_start = 0;
    for b_idx in 0..4 {
        let expected_block_rows = if b_idx == 3 { 4 } else { 7 };
        for (col_idx, _col_def) in schema.columns().iter().enumerate() {
            let block_meta = reader.block_meta(col_idx, b_idx).unwrap();
            assert_eq!(block_meta.row_start, current_row_start as u64);
            assert_eq!(block_meta.row_count, expected_block_rows as u32);

            let cv: ColumnVector = reader.read_block(col_idx, b_idx).unwrap();
            assert_eq!(cv.len(), expected_block_rows);

            for r in 0..expected_block_rows {
                let actual_val = cv.get(r);
                let expected_val = rows[current_row_start + r].get(col_idx);

                if col_idx == 7 {
                    match (actual_val.as_ref(), expected_val) {
                        (
                            Some(Value::Decimal {
                                value: actual_value,
                                precision: actual_precision,
                                scale: actual_scale,
                            }),
                            Some(Value::Decimal {
                                value: expected_value,
                                precision: expected_precision,
                                scale: expected_scale,
                            }),
                        ) => {
                            assert_eq!(actual_value, expected_value);
                            assert_eq!(actual_precision, expected_precision);
                            assert_eq!(actual_scale, expected_scale);
                        }
                        (actual, expected) => {
                            panic!("expected decimal values, got actual {actual:?}, expected {expected:?}");
                        }
                    }
                } else {
                    assert_eq!(actual_val.as_ref(), expected_val);
                }
            }
        }
        current_row_start += expected_block_rows;
    }
}

#[test]
fn test_exact_null_and_float_bit_preservation() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("float_bits.col");
    let schema = Schema::new(vec![
        test_col("f_nullable", DataType::Float64, true),
        test_col("f_required", DataType::Float64, false),
    ])
    .unwrap();

    let special_floats = vec![
        0.0f64,
        -0.0f64,
        f64::NAN,
        f64::from_bits(0x7ff8_0000_1234_5678), // NaN with specific payload
        f64::INFINITY,
        f64::NEG_INFINITY,
        1.23456789012345e-300,
        f64::MIN,
        f64::MAX,
    ];

    let mut rows = Vec::new();
    for (i, &f) in special_floats.iter().enumerate() {
        let f_null = if i % 2 == 0 {
            Value::Null
        } else {
            Value::Float64(f)
        };
        rows.push(Row::new(vec![f_null, Value::Float64(f)]));
    }

    let opts = SegmentOptions::new().with_rows_per_block(4);
    let meta = SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    assert_eq!(meta.row_count, special_floats.len() as u64);

    let mut reader = SegmentReader::open(&path).unwrap();
    let num_blocks = reader.block_count();

    let mut global_row_idx = 0;
    for b_idx in 0..num_blocks {
        let cv_nullable = reader.read_block(0, b_idx).unwrap();
        let cv_required = reader.read_block(1, b_idx).unwrap();

        for r in 0..cv_nullable.len() {
            let orig_f = special_floats[global_row_idx];
            let val_req = cv_required.get(r).unwrap();
            match val_req {
                Value::Float64(read_f) => {
                    assert_eq!(
                        read_f.to_bits(),
                        orig_f.to_bits(),
                        "float bit pattern must match exactly for row {global_row_idx}"
                    );
                    if orig_f.is_sign_negative() && orig_f == 0.0 {
                        assert!(read_f.is_sign_negative(), "-0.0 sign bit must be 1");
                    }
                }
                other => panic!("expected Float64, got {other:?}"),
            }

            let val_null = cv_nullable.get(r).unwrap();
            if global_row_idx % 2 == 0 {
                assert_eq!(val_null, Value::Null);
            } else {
                match val_null {
                    Value::Float64(read_f) => {
                        assert_eq!(read_f.to_bits(), orig_f.to_bits());
                    }
                    other => panic!("expected Float64, got {other:?}"),
                }
            }
            global_row_idx += 1;
        }
    }
}

#[test]
fn test_strings_and_bytes_empty_and_embedded_nul() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nul_strings.col");
    let schema = Schema::new(vec![
        test_col("s", DataType::String, true),
        test_col("b", DataType::Bytes, true),
    ])
    .unwrap();

    let rows = vec![
        Row::new(vec![Value::String("".into()), Value::Bytes(vec![])]),
        Row::new(vec![
            Value::String("hello\0world\0with\0nuls".into()),
            Value::Bytes(vec![0x00, 0x01, 0x00, 0x00, 0xff]),
        ]),
        Row::new(vec![Value::Null, Value::Null]),
        Row::new(vec![
            Value::String("utf8_🚀_unicode".into()),
            Value::Bytes(b"plain ascii bytes".to_vec()),
        ]),
    ];

    let opts = SegmentOptions::new().with_rows_per_block(10);
    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    let cv_s = reader.read_block(0, 0).unwrap();
    let cv_b = reader.read_block(1, 0).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(cv_s.get(i).as_ref(), row.get(0));
        assert_eq!(cv_b.get(i).as_ref(), row.get(1));
    }
}

#[test]
fn test_plain_and_dictionary_paths() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("encodings.col");
    let schema = Schema::new(vec![
        test_col("c_plain", DataType::String, false),
        test_col("c_dict", DataType::String, false),
    ])
    .unwrap();

    // c_plain will have unique long strings where dictionary overhead makes it larger
    // c_dict will have repeated identical strings where dictionary is much smaller
    let mut rows = Vec::new();
    for i in 0..100 {
        rows.push(Row::new(vec![
            Value::String(format!("unique_word_index_{i:04}")),
            Value::String("highly_repetitive_value_for_dict_selection".into()),
        ]));
    }

    let opts = SegmentOptions::new().with_rows_per_block(100);
    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    let meta_plain = reader.block_meta(0, 0).unwrap();
    let meta_dict = reader.block_meta(1, 0).unwrap();

    assert_eq!(meta_plain.encoding, ColumnEncoding::Plain);
    assert_eq!(meta_dict.encoding, ColumnEncoding::Dictionary);

    let cv_plain = reader.read_block(0, 0).unwrap();
    let cv_dict = reader.read_block(1, 0).unwrap();

    for (i, row) in rows.iter().enumerate() {
        assert_eq!(cv_plain.get(i).as_ref(), row.get(0));
        assert_eq!(cv_dict.get(i).as_ref(), row.get(1));
    }
}

#[test]
fn test_raw_and_compressed_paths() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("compression.col");
    let schema = Schema::new(vec![
        test_col("c_raw", DataType::Int32, false),
        test_col("c_comp", DataType::String, false),
    ])
    .unwrap();

    // 5 rows of small ints will not compress >= 10%
    // 500 rows of repeated strings will easily compress
    let mut rows = Vec::new();
    for i in 0..500 {
        rows.push(Row::new(vec![
            Value::Int32(i),
            Value::String("compress_me_compress_me_compress_me_12345".into()),
        ]));
    }

    let opts = SegmentOptions::new().with_rows_per_block(500);
    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    let meta_comp = reader.block_meta(1, 0).unwrap();

    // The compressed column must have stored_bytes < raw_bytes
    assert!(
        meta_comp.stored_bytes < meta_comp.raw_bytes,
        "compressed column stored {} should be < raw {}",
        meta_comp.stored_bytes,
        meta_comp.raw_bytes
    );

    let cv_raw = reader.read_block(0, 0).unwrap();
    let cv_comp = reader.read_block(1, 0).unwrap();
    assert_eq!(cv_raw.len(), 500);
    assert_eq!(cv_comp.len(), 500);
}

#[test]
fn test_metadata_and_ordinal_alignment() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("alignment.col");
    let schema = full_schema();

    let mut rows = Vec::new();
    for i in 0..50 {
        rows.push(Row::new(vec![
            Value::Bool(i % 3 == 0),
            Value::Int32(i),
            Value::Int64((i as i64) * 10),
            Value::Float64(i as f64),
            Value::String(format!("val_{i}")),
            Value::Bytes(vec![i as u8]),
            Value::Timestamp(i as i64),
            Value::Decimal {
                value: (i as i64) * 10_000,
                precision: 18,
                scale: 4,
            },
        ]));
    }

    let opts = SegmentOptions::new().with_rows_per_block(13);
    SegmentWriter::write(&path, &schema, rows, &opts).unwrap();

    let reader = SegmentReader::open(&path).unwrap();
    assert_eq!(reader.block_count(), 4); // 13, 13, 13, 11

    let expected_slices = [(0, 13), (13, 13), (26, 13), (39, 11)];
    for (b_idx, &(exp_start, exp_count)) in expected_slices.iter().enumerate() {
        for col_idx in 0..schema.len() {
            let meta = reader.block_meta(col_idx, b_idx).unwrap();
            assert_eq!(meta.row_start, exp_start);
            assert_eq!(meta.row_count, exp_count);
            assert!(meta.has_not_null);
            assert!(meta.min_value.is_some());
            assert!(meta.max_value.is_some());
            assert!(meta.min_value.as_ref().unwrap() <= meta.max_value.as_ref().unwrap());
        }
    }
}

#[test]
fn test_corruption_header_magic() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_hdr.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(1)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    flip_byte_at(&path, 0);
    assert!(matches!(
        SegmentReader::open(&path),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_trailer_magic() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_trailer.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(1)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let len = std::fs::metadata(&path).unwrap().len();
    flip_byte_at(&path, len - 2);
    assert!(matches!(
        SegmentReader::open(&path),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_footer_crc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_footer_crc.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(1)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let len = std::fs::metadata(&path).unwrap().len();
    // Trailer is 12 bytes: [footer_len: 4, footer_crc: 4, magic: 4]
    // CRC byte is at len - 8
    flip_byte_at(&path, len - 6);
    assert!(matches!(
        SegmentReader::open(&path),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_footer_payload() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_footer_payload.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(1)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let len = std::fs::metadata(&path).unwrap().len();
    // Byte inside footer payload
    flip_byte_at(&path, len - 16);
    assert!(matches!(
        SegmentReader::open(&path),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_file_truncated() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("truncated.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(1)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let len = std::fs::metadata(&path).unwrap().len();
    let f = OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(len - 5).unwrap();
    f.sync_all().unwrap();
    drop(f);

    assert!(matches!(
        SegmentReader::open(&path),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_frame_crc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_frame_crc.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(100)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    let block = reader.block_meta(0, 0).unwrap();
    let body_offset = block.offset + FRAME_HEADER_LEN as u64;

    // Flip byte in block body
    flip_byte_at(&path, body_offset);

    assert!(matches!(
        reader.read_block(0, 0),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_frame_length_fields() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_length_field.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(100)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    let block = reader.block_meta(0, 0).unwrap();
    // Flip byte in frame header's raw_payload_len (offset 21..25)
    flip_byte_at(&path, block.offset + 22);

    assert!(matches!(
        reader.read_block(0, 0),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_encoding_tag() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_encoding_tag.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![Row::new(vec![Value::Int32(100)])];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    let block = reader.block_meta(0, 0).unwrap();
    // Encoding byte is at offset 16 in frame header
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(block.offset + 16)).unwrap();
    file.write_all(&[99u8]).unwrap(); // invalid encoding tag
    file.sync_all().unwrap();
    drop(file);

    assert!(matches!(
        reader.read_block(0, 0),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_zone_metadata() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("corrupt_zone.col");
    let schema = Schema::new(vec![test_col("c", DataType::Int32, false)]).unwrap();
    let rows = vec![
        Row::new(vec![Value::Int32(10)]),
        Row::new(vec![Value::Int32(20)]),
    ];
    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    // Read full file bytes
    let mut bytes = std::fs::read(&path).unwrap();
    let len = bytes.len();
    let footer_len = u32::from_le_bytes(bytes[len - 12..len - 8].try_into().unwrap()) as usize;
    let footer_offset = len - 12 - footer_len;

    // In footer payload:
    // format_version: 2
    // schema_len: 4
    // schema_json: schema_len
    // total_rows: 8
    // col_count: 4
    // col_idx: 4
    // block_count: 4
    // block entry: offset:8, frame_len:4, row_start:8, row_count:4, enc:1, raw:4, stored:4, crc:4, has_null:1, has_not_null:1
    // followed by min_val: 4 (i32), max_val: 4 (i32)
    // Let's swap min_val and max_val so min_val > max_val
    // Find min_val offset:
    let min_val_offset = len - 12 - 8;
    let max_val_offset = len - 12 - 4;
    // Set min_val to 999 and max_val to 0
    bytes[min_val_offset..min_val_offset + 4].copy_from_slice(&999i32.to_le_bytes());
    bytes[max_val_offset..max_val_offset + 4].copy_from_slice(&0i32.to_le_bytes());

    // Recompute footer CRC
    let new_crc = crc32c::crc32c(&bytes[footer_offset..len - 12]);
    bytes[len - 8..len - 4].copy_from_slice(&new_crc.to_le_bytes());

    std::fs::write(&path, &bytes).unwrap();

    // Open must detect that min_value > max_value and return Corruption!
    let res = SegmentReader::open(&path);
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_corruption_compressed_payload_with_valid_crc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("corrupt_zstd.col");
    let schema = Schema::new(vec![test_col("c", DataType::String, false)]).unwrap();
    // 200 identical strings will definitely compress with zstd
    let rows: Vec<Row> = (0..200)
        .map(|_| Row::new(vec![Value::String("compress_me_compress_me_12345".into())]))
        .collect();

    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    let block = reader.block_meta(0, 0).unwrap();
    assert!(block.stored_bytes < block.raw_bytes, "must be compressed");

    let body_start = (block.offset + FRAME_HEADER_LEN as u64) as usize;
    let body_end = (block.offset + block.frame_len as u64) as usize;

    // Flip bytes in the compressed payload (which is in the body after the null bitmap)
    // Null bitmap length for 200 rows is ceil(200/8) = 25 bytes.
    let compressed_start = body_start + 25;
    bytes[compressed_start + 5] ^= 0xff;

    // Recompute block frame CRC so CRC check passes, but decompression fails!
    let new_frame_crc = crc32c::crc32c(&bytes[body_start..body_end]);
    let frame_crc_offset = (block.offset + 29) as usize;
    bytes[frame_crc_offset..frame_crc_offset + 4].copy_from_slice(&new_frame_crc.to_le_bytes());

    // Also update CRC in the footer so footer validation passes!
    let len = bytes.len();
    let footer_len = u32::from_le_bytes(bytes[len - 12..len - 8].try_into().unwrap()) as usize;
    let footer_offset = len - 12 - footer_len;

    // In footer, find the CRC field of block 0 and update it:
    // Format: format_version(2) + schema_len(4) + schema_json + total_rows(8) + col_count(4) + col_idx(4) + block_count(4)
    // + offset(8) + frame_len(4) + row_start(8) + row_count(4) + enc(1) + raw(4) + stored(4) + crc(4)
    let schema_len = u32::from_le_bytes(
        bytes[footer_offset + 2..footer_offset + 6]
            .try_into()
            .unwrap(),
    ) as usize;
    let footer_crc_pos =
        footer_offset + 2 + 4 + schema_len + 8 + 4 + 4 + 4 + 8 + 4 + 8 + 4 + 1 + 4 + 4;
    bytes[footer_crc_pos..footer_crc_pos + 4].copy_from_slice(&new_frame_crc.to_le_bytes());

    // Recompute footer CRC
    let new_footer_crc = crc32c::crc32c(&bytes[footer_offset..len - 12]);
    bytes[len - 8..len - 4].copy_from_slice(&new_footer_crc.to_le_bytes());

    std::fs::write(&path, &bytes).unwrap();

    // Reader open passes, but read_block fails when decompressing!
    let mut reader_corrupt = SegmentReader::open(&path).unwrap();
    let res = reader_corrupt.read_block(0, 0);
    assert!(matches!(res, Err(HtapError::Corruption(_))));
}

#[test]
fn test_corruption_decimal_payload_outside_declared_precision() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bad_decimal_value.col");
    let schema = Schema::new(vec![test_col(
        "c",
        DataType::Decimal {
            precision: 2,
            scale: 0,
        },
        false,
    )])
    .unwrap();
    let rows = vec![Row::new(vec![Value::Decimal {
        value: 12,
        precision: 2,
        scale: 0,
    }])];

    SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

    let mut bytes = std::fs::read(&path).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    let block = reader.block_meta(0, 0).unwrap();
    let body_start = (block.offset + FRAME_HEADER_LEN as u64) as usize;
    let body_end = (block.offset + block.frame_len as u64) as usize;

    // One row has a one-byte null bitmap followed by the scaled i64 payload.
    bytes[body_start + 1..body_start + 9].copy_from_slice(&999i64.to_le_bytes());

    // Repair the frame CRC and its footer copy so only decimal validation can fail.
    let new_frame_crc = crc32c::crc32c(&bytes[body_start..body_end]);
    let frame_crc_offset = (block.offset + 29) as usize;
    bytes[frame_crc_offset..frame_crc_offset + 4].copy_from_slice(&new_frame_crc.to_le_bytes());

    let len = bytes.len();
    let footer_len = u32::from_le_bytes(bytes[len - 12..len - 8].try_into().unwrap()) as usize;
    let footer_offset = len - 12 - footer_len;
    let schema_len = u32::from_le_bytes(
        bytes[footer_offset + 2..footer_offset + 6]
            .try_into()
            .unwrap(),
    ) as usize;
    let footer_crc_pos =
        footer_offset + 2 + 4 + schema_len + 8 + 4 + 4 + 4 + 8 + 4 + 8 + 4 + 1 + 4 + 4;
    bytes[footer_crc_pos..footer_crc_pos + 4].copy_from_slice(&new_frame_crc.to_le_bytes());

    let new_footer_crc = crc32c::crc32c(&bytes[footer_offset..len - 12]);
    bytes[len - 8..len - 4].copy_from_slice(&new_footer_crc.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();

    let mut corrupt_reader = SegmentReader::open(&path).unwrap();
    assert!(matches!(
        corrupt_reader.read_block(0, 0),
        Err(HtapError::Corruption(_))
    ));
}

#[test]
fn test_corruption_footer_schema_out_of_range_decimal_type() {
    let dir = tempdir().unwrap();

    let cases = [
        ("zero_precision", 1, 0, "precision", "0", Vec::new()),
        (
            "precision_above_max",
            18,
            0,
            "precision",
            "19",
            vec![Row::new(vec![Value::Decimal {
                value: 1,
                precision: 18,
                scale: 0,
            }])],
        ),
        (
            "scale_larger_than_precision",
            2,
            1,
            "scale",
            "3",
            vec![Row::new(vec![Value::Decimal {
                value: 1,
                precision: 2,
                scale: 1,
            }])],
        ),
    ];

    for (name, precision, scale, field, replacement, rows) in cases {
        let path = dir.path().join(format!("{name}.col"));
        let schema = Schema::new(vec![test_col(
            "c",
            DataType::Decimal { precision, scale },
            false,
        )])
        .unwrap();

        SegmentWriter::write(&path, &schema, rows, &SegmentOptions::new()).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        replace_footer_decimal_schema_number(&mut bytes, field, replacement);
        std::fs::write(&path, &bytes).unwrap();

        assert!(
            matches!(SegmentReader::open(&path), Err(HtapError::Corruption(_))),
            "{name} decimal schema must be rejected"
        );
    }
}

#[test]
fn test_nullable_decimal_roundtrip_across_blocks_and_null_zone_map() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("nullable_decimal.col");
    let schema = Schema::new(vec![test_col(
        "c",
        DataType::Decimal {
            precision: 5,
            scale: 2,
        },
        true,
    )])
    .unwrap();
    let rows = vec![
        Row::new(vec![Value::Null]),
        Row::new(vec![Value::Null]),
        Row::new(vec![Value::Decimal {
            value: 1234,
            precision: 5,
            scale: 2,
        }]),
        Row::new(vec![Value::Null]),
        Row::new(vec![Value::Decimal {
            value: -99,
            precision: 5,
            scale: 2,
        }]),
        Row::new(vec![Value::Decimal {
            value: 0,
            precision: 5,
            scale: 2,
        }]),
    ];

    let opts = SegmentOptions::new().with_rows_per_block(2);
    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();

    let mut reader = SegmentReader::open(&path).unwrap();
    assert_eq!(reader.block_count(), 3);

    let all_null_block = reader.block_meta(0, 0).unwrap();
    assert!(all_null_block.has_null);
    assert!(!all_null_block.has_not_null);
    assert!(all_null_block.min_value.is_none());
    assert!(all_null_block.max_value.is_none());

    for block_idx in 0..reader.block_count() {
        let block = reader.read_block(0, block_idx).unwrap();
        for row_idx in 0..block.len() {
            let global_row = block_idx * 2 + row_idx;
            match (
                block.get(row_idx).unwrap(),
                rows[global_row].get(0).unwrap(),
            ) {
                (Value::Null, Value::Null) => {}
                (
                    Value::Decimal {
                        value: actual_value,
                        precision: actual_precision,
                        scale: actual_scale,
                    },
                    Value::Decimal {
                        value: expected_value,
                        precision: expected_precision,
                        scale: expected_scale,
                    },
                ) => {
                    assert_eq!(actual_value, *expected_value);
                    assert_eq!(actual_precision, *expected_precision);
                    assert_eq!(actual_scale, *expected_scale);
                }
                (actual, expected) => {
                    panic!(
                        "unexpected decimal roundtrip values: actual {actual:?}, expected {expected:?}"
                    );
                }
            }
        }
    }
}

fn replace_footer_decimal_schema_number(bytes: &mut Vec<u8>, field: &str, replacement: &str) {
    let old_len = bytes.len();
    let old_footer_len =
        u32::from_le_bytes(bytes[old_len - 12..old_len - 8].try_into().unwrap()) as usize;
    let footer_offset = old_len - 12 - old_footer_len;
    let schema_len = u32::from_le_bytes(
        bytes[footer_offset + 2..footer_offset + 6]
            .try_into()
            .unwrap(),
    ) as usize;
    let schema_start = footer_offset + 6;
    let schema_end = schema_start + schema_len;
    let marker = format!("\"{field}\":").into_bytes();
    let marker_offset = bytes[schema_start..schema_end]
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let value_start = schema_start + marker_offset + marker.len();
    let value_end = bytes[value_start..schema_end]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map(|offset| value_start + offset)
        .unwrap();

    bytes.splice(value_start..value_end, replacement.bytes());

    let new_len = bytes.len();
    let length_delta = new_len as isize - old_len as isize;
    let new_schema_len = (schema_len as isize + length_delta) as u32;
    bytes[footer_offset + 2..footer_offset + 6].copy_from_slice(&new_schema_len.to_le_bytes());

    let new_footer_len = (old_footer_len as isize + length_delta) as u32;
    bytes[new_len - 12..new_len - 8].copy_from_slice(&new_footer_len.to_le_bytes());

    let new_footer_crc = crc32c::crc32c(&bytes[footer_offset..new_len - 12]);
    bytes[new_len - 8..new_len - 4].copy_from_slice(&new_footer_crc.to_le_bytes());
}
