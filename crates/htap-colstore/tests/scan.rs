//! Integration tests for columnar segment vectorized scan with zone-map pushdown.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use htap_colstore::{
    can_block_contain_matches, Predicate, ScanRequest, ScanResult, SegmentOptions, SegmentReader,
    SegmentWriter, FRAME_HEADER_LEN,
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

fn full_schema() -> Schema {
    Schema::new(vec![
        test_col("c_bool", DataType::Bool, true),
        test_col("c_int32", DataType::Int32, true),
        test_col("c_int64", DataType::Int64, true),
        test_col("c_float64", DataType::Float64, true),
        test_col("c_string", DataType::String, true),
        test_col("c_bytes", DataType::Bytes, true),
        test_col("c_timestamp", DataType::Timestamp, true),
    ])
    .unwrap()
}

/// Helper that evaluates a scan request naively against in-memory rows.
fn reference_scan(
    schema: &Schema,
    rows: &[Row],
    request: &ScanRequest,
) -> (Vec<usize>, Vec<Vec<Value>>) {
    let mut selected_indices = Vec::new();
    let mut projected_cols: Vec<Vec<Value>> = vec![Vec::new(); request.projection.len()];

    for (row_idx, row) in rows.iter().enumerate() {
        let matches = match &request.predicate {
            None => true,
            Some(pred) => {
                let val = row.get(pred.column()).unwrap();
                pred.matches(val)
            }
        };

        if matches {
            selected_indices.push(row_idx);
            for (p_idx, &col_idx) in request.projection.iter().enumerate() {
                let val = row.get(col_idx).unwrap().clone();
                projected_cols[p_idx].push(val);
            }
        }
    }

    let _ = schema;
    (selected_indices, projected_cols)
}

/// Verifies that ScanResult matches reference scan output.
fn verify_scan_against_reference(
    reader: &SegmentReader,
    rows: &[Row],
    request: &ScanRequest,
    result: &ScanResult,
) {
    let (ref_indices, ref_cols) = reference_scan(reader.schema(), rows, request);

    assert_eq!(result.total_rows(), ref_indices.len());
    assert_eq!(result.stats.returned_rows, ref_indices.len());

    let mut actual_global_row_ids = Vec::new();
    let mut actual_cols: Vec<Vec<Value>> = vec![Vec::new(); request.projection.len()];

    for batch in &result.batches {
        // No empty batches allowed
        assert!(!batch.is_empty(), "returned batch must not be empty");
        assert_eq!(batch.num_columns(), request.projection.len());

        for &rel_id in &batch.row_ids {
            let global_id = batch.row_start + rel_id as u64;
            actual_global_row_ids.push(global_id as usize);
        }

        for (p_idx, col_vec) in batch.columns.iter().enumerate() {
            assert_eq!(col_vec.len(), batch.num_rows());
            assert_eq!(col_vec.validity().len(), batch.num_rows());
            for r in 0..col_vec.len() {
                actual_cols[p_idx].push(col_vec.get(r).unwrap());
            }
        }
    }

    assert_eq!(actual_global_row_ids, ref_indices);
    for (p_idx, expected_vals) in ref_cols.iter().enumerate() {
        assert_eq!(
            &actual_cols[p_idx], expected_vals,
            "projected column {p_idx} mismatch"
        );
    }
}

#[test]
fn test_reference_equivalent_scans_all_predicates_and_shared_types() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("all_predicates.col");
    let schema = full_schema();

    // 30 rows with rows_per_block = 6 => 5 blocks
    let opts = SegmentOptions::new().with_rows_per_block(6);

    let mut rows = Vec::new();
    for i in 0..30 {
        let is_null = i % 5 == 0;
        let row = Row::new(vec![
            if is_null {
                Value::Null
            } else {
                Value::Bool(i % 2 == 0)
            },
            if is_null {
                Value::Null
            } else {
                Value::Int32(i * 10 - 150)
            },
            if is_null {
                Value::Null
            } else {
                Value::Int64((i as i64) * 1000 - 15_000)
            },
            if is_null {
                Value::Null
            } else {
                Value::Float64((i as f64) * 0.5 - 7.5)
            },
            if is_null {
                Value::Null
            } else {
                Value::String(format!("str_{:02}", i))
            },
            if is_null {
                Value::Null
            } else {
                Value::Bytes(vec![i as u8, (i + 1) as u8])
            },
            if is_null {
                Value::Null
            } else {
                Value::Timestamp(1_700_000_000 + (i as i64) * 100)
            },
        ]);
        rows.push(row);
    }

    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // Test each predicate type across applicable columns
    let test_requests = vec![
        // Int32: Eq, Lt, Lte, Gt, Gte, IsNull, IsNotNull
        ScanRequest::new(
            vec![1],
            Some(Predicate::Eq {
                column: 1,
                value: Value::Int32(0),
            }),
        ),
        ScanRequest::new(
            vec![1, 4],
            Some(Predicate::Lt {
                column: 1,
                value: Value::Int32(-50),
            }),
        ),
        ScanRequest::new(
            vec![0, 1, 2],
            Some(Predicate::Lte {
                column: 1,
                value: Value::Int32(50),
            }),
        ),
        ScanRequest::new(
            vec![1, 6],
            Some(Predicate::Gt {
                column: 1,
                value: Value::Int32(20),
            }),
        ),
        ScanRequest::new(
            vec![1],
            Some(Predicate::Gte {
                column: 1,
                value: Value::Int32(0),
            }),
        ),
        ScanRequest::new(vec![1], Some(Predicate::IsNull { column: 1 })),
        ScanRequest::new(vec![1], Some(Predicate::IsNotNull { column: 1 })),
        // Int64: Eq, Lt, Gt
        ScanRequest::new(
            vec![2],
            Some(Predicate::Eq {
                column: 2,
                value: Value::Int64(5000),
            }),
        ),
        ScanRequest::new(
            vec![2],
            Some(Predicate::Lt {
                column: 2,
                value: Value::Int64(0),
            }),
        ),
        ScanRequest::new(
            vec![2],
            Some(Predicate::Gt {
                column: 2,
                value: Value::Int64(10_000),
            }),
        ),
        // Float64: Lte, Gte
        ScanRequest::new(
            vec![3],
            Some(Predicate::Lte {
                column: 3,
                value: Value::Float64(0.0),
            }),
        ),
        ScanRequest::new(
            vec![3],
            Some(Predicate::Gte {
                column: 3,
                value: Value::Float64(2.5),
            }),
        ),
        // Bool: Eq(true), Eq(false), IsNull
        ScanRequest::new(
            vec![0],
            Some(Predicate::Eq {
                column: 0,
                value: Value::Bool(true),
            }),
        ),
        ScanRequest::new(
            vec![0],
            Some(Predicate::Eq {
                column: 0,
                value: Value::Bool(false),
            }),
        ),
        ScanRequest::new(vec![0], Some(Predicate::IsNull { column: 0 })),
        // String: Eq, Lt, Gt
        ScanRequest::new(
            vec![4],
            Some(Predicate::Eq {
                column: 4,
                value: Value::String("str_12".into()),
            }),
        ),
        ScanRequest::new(
            vec![4],
            Some(Predicate::Lt {
                column: 4,
                value: Value::String("str_10".into()),
            }),
        ),
        ScanRequest::new(
            vec![4],
            Some(Predicate::Gt {
                column: 4,
                value: Value::String("str_25".into()),
            }),
        ),
        // Bytes: Eq, Lt, Gt
        ScanRequest::new(
            vec![5],
            Some(Predicate::Eq {
                column: 5,
                value: Value::Bytes(vec![12, 13]),
            }),
        ),
        ScanRequest::new(
            vec![5],
            Some(Predicate::Lt {
                column: 5,
                value: Value::Bytes(vec![5, 6]),
            }),
        ),
        // Timestamp: Gt, Lte
        ScanRequest::new(
            vec![6],
            Some(Predicate::Gt {
                column: 6,
                value: Value::Timestamp(1_700_001_000),
            }),
        ),
        ScanRequest::new(
            vec![6],
            Some(Predicate::Lte {
                column: 6,
                value: Value::Timestamp(1_700_000_500),
            }),
        ),
    ];

    for req in test_requests {
        let result = reader.scan(&req).unwrap();
        verify_scan_against_reference(&reader, &rows, &req, &result);
        assert_eq!(
            result.stats.candidate_blocks,
            result.stats.skipped_blocks + result.stats.decoded_blocks
        );
    }
}

#[test]
fn test_all_four_zone_map_null_states() {
    let dir = tempdir().unwrap();
    let schema = Schema::new(vec![
        test_col("c_val", DataType::Int32, true),
        test_col("c_tag", DataType::String, true),
    ])
    .unwrap();

    // 1. Empty segment behavior
    let empty_path = dir.path().join("empty.col");
    let opts = SegmentOptions::new().with_rows_per_block(4);
    SegmentWriter::write(&empty_path, &schema, Vec::<Row>::new(), &opts).unwrap();
    let empty_reader = SegmentReader::open(&empty_path).unwrap();
    let scan_empty = empty_reader.scan(&ScanRequest::new(vec![0], None)).unwrap();
    assert_eq!(scan_empty.batches.len(), 0);
    assert_eq!(scan_empty.stats.candidate_blocks, 0);
    assert_eq!(scan_empty.stats.skipped_blocks, 0);
    assert_eq!(scan_empty.stats.decoded_blocks, 0);
    assert_eq!(scan_empty.stats.returned_rows, 0);

    // Segment with 3 distinct blocks:
    // Block 0: All-null
    // Block 1: Null-free (10..14)
    // Block 2: Mixed null/non-null (null, 50, null, 55)
    let mixed_path = dir.path().join("mixed_states.col");
    let rows = vec![
        // Block 0 (all null)
        Row::new(vec![Value::Null, Value::Null]),
        Row::new(vec![Value::Null, Value::Null]),
        Row::new(vec![Value::Null, Value::Null]),
        Row::new(vec![Value::Null, Value::Null]),
        // Block 1 (null-free: min 10, max 13)
        Row::new(vec![Value::Int32(10), Value::String("a".into())]),
        Row::new(vec![Value::Int32(11), Value::String("b".into())]),
        Row::new(vec![Value::Int32(12), Value::String("c".into())]),
        Row::new(vec![Value::Int32(13), Value::String("d".into())]),
        // Block 2 (mixed: min 50, max 55)
        Row::new(vec![Value::Null, Value::String("e".into())]),
        Row::new(vec![Value::Int32(50), Value::Null]),
        Row::new(vec![Value::Null, Value::String("g".into())]),
        Row::new(vec![Value::Int32(55), Value::String("h".into())]),
    ];

    SegmentWriter::write(&mixed_path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&mixed_path).unwrap();
    assert_eq!(reader.block_count(), 3);

    // Verify zone map states on column 0 metadata
    let b0 = reader.block_meta(0, 0).unwrap();
    assert!(b0.has_null && !b0.has_not_null);
    let b1 = reader.block_meta(0, 1).unwrap();
    assert!(!b1.has_null && b1.has_not_null);
    let b2 = reader.block_meta(0, 2).unwrap();
    assert!(b2.has_null && b2.has_not_null);

    // Verify exact conservative pruning table for Block 0 (all-null):
    // Eq: all-null skips
    assert!(!can_block_contain_matches(
        b0,
        &Predicate::Eq {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    // Lt / Lte: all-null skips
    assert!(!can_block_contain_matches(
        b0,
        &Predicate::Lt {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    assert!(!can_block_contain_matches(
        b0,
        &Predicate::Lte {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    // Gt / Gte: any block containing null is retained conservatively
    assert!(can_block_contain_matches(
        b0,
        &Predicate::Gt {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    assert!(can_block_contain_matches(
        b0,
        &Predicate::Gte {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    // IsNull: retained
    assert!(can_block_contain_matches(
        b0,
        &Predicate::IsNull { column: 0 }
    ));
    // IsNotNull: skipped
    assert!(!can_block_contain_matches(
        b0,
        &Predicate::IsNotNull { column: 0 }
    ));

    // Verify exact conservative pruning table for Block 1 (null-free: min 10, max 13):
    // Eq(v): non-null-only skips if v < min || v > max
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::Eq {
            column: 0,
            value: Value::Int32(9)
        }
    ));
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::Eq {
            column: 0,
            value: Value::Int32(14)
        }
    ));
    assert!(can_block_contain_matches(
        b1,
        &Predicate::Eq {
            column: 0,
            value: Value::Int32(12)
        }
    ));
    // Lt(v) / Lte(v): skip when min >= v / min > v
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::Lt {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    assert!(can_block_contain_matches(
        b1,
        &Predicate::Lt {
            column: 0,
            value: Value::Int32(11)
        }
    ));
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::Lte {
            column: 0,
            value: Value::Int32(9)
        }
    ));
    assert!(can_block_contain_matches(
        b1,
        &Predicate::Lte {
            column: 0,
            value: Value::Int32(10)
        }
    ));
    // Gt(v) / Gte(v): null-free blocks skip when max <= v / max < v
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::Gt {
            column: 0,
            value: Value::Int32(13)
        }
    ));
    assert!(can_block_contain_matches(
        b1,
        &Predicate::Gt {
            column: 0,
            value: Value::Int32(12)
        }
    ));
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::Gte {
            column: 0,
            value: Value::Int32(14)
        }
    ));
    assert!(can_block_contain_matches(
        b1,
        &Predicate::Gte {
            column: 0,
            value: Value::Int32(13)
        }
    ));
    // IsNull: skipped (has_null == false)
    assert!(!can_block_contain_matches(
        b1,
        &Predicate::IsNull { column: 0 }
    ));
    // IsNotNull: retained (has_not_null == true)
    assert!(can_block_contain_matches(
        b1,
        &Predicate::IsNotNull { column: 0 }
    ));

    // Verify exact conservative pruning table for Block 2 (mixed: has_null == true, min 50, max 55):
    // Eq(v): any null-containing block is retained conservatively
    assert!(can_block_contain_matches(
        b2,
        &Predicate::Eq {
            column: 0,
            value: Value::Int32(99)
        }
    ));
    // Gt(v) / Gte(v): any block containing NULL is retained
    assert!(can_block_contain_matches(
        b2,
        &Predicate::Gt {
            column: 0,
            value: Value::Int32(99)
        }
    ));
    assert!(can_block_contain_matches(
        b2,
        &Predicate::Gte {
            column: 0,
            value: Value::Int32(99)
        }
    ));
    // Lt(v) / Lte(v): skip when min >= v / min > v
    assert!(!can_block_contain_matches(
        b2,
        &Predicate::Lt {
            column: 0,
            value: Value::Int32(50)
        }
    ));
    assert!(can_block_contain_matches(
        b2,
        &Predicate::Lt {
            column: 0,
            value: Value::Int32(51)
        }
    ));
    assert!(!can_block_contain_matches(
        b2,
        &Predicate::Lte {
            column: 0,
            value: Value::Int32(49)
        }
    ));
    assert!(can_block_contain_matches(
        b2,
        &Predicate::Lte {
            column: 0,
            value: Value::Int32(50)
        }
    ));
    // IsNull: retained
    assert!(can_block_contain_matches(
        b2,
        &Predicate::IsNull { column: 0 }
    ));
    // IsNotNull: retained
    assert!(can_block_contain_matches(
        b2,
        &Predicate::IsNotNull { column: 0 }
    ));

    // Test scan with IsNull on column 0:
    // Block 0: all null (4 matches)
    // Block 1: null-free (skipped via zone-map!)
    // Block 2: mixed (2 nulls match)
    let is_null_scan = reader
        .scan(&ScanRequest::new(
            vec![0],
            Some(Predicate::IsNull { column: 0 }),
        ))
        .unwrap();
    assert_eq!(is_null_scan.stats.candidate_blocks, 3);
    assert_eq!(is_null_scan.stats.skipped_blocks, 1);
    assert_eq!(is_null_scan.stats.decoded_blocks, 2);
    assert_eq!(is_null_scan.stats.returned_rows, 6);
    verify_scan_against_reference(
        &reader,
        &rows,
        &ScanRequest::new(vec![0], Some(Predicate::IsNull { column: 0 })),
        &is_null_scan,
    );

    // Test scan with IsNotNull on column 0:
    // Block 0: all-null (skipped via zone-map!)
    // Block 1: null-free (4 matches)
    // Block 2: mixed (2 non-nulls match)
    let is_not_null_scan = reader
        .scan(&ScanRequest::new(
            vec![0],
            Some(Predicate::IsNotNull { column: 0 }),
        ))
        .unwrap();
    assert_eq!(is_not_null_scan.stats.candidate_blocks, 3);
    assert_eq!(is_not_null_scan.stats.skipped_blocks, 1);
    assert_eq!(is_not_null_scan.stats.decoded_blocks, 2);
    assert_eq!(is_not_null_scan.stats.returned_rows, 6);
    verify_scan_against_reference(
        &reader,
        &rows,
        &ScanRequest::new(vec![0], Some(Predicate::IsNotNull { column: 0 })),
        &is_not_null_scan,
    );
}

#[test]
fn test_exact_projected_column_order_and_stable_row_ids() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("proj_order.col");
    let schema = Schema::new(vec![
        test_col("c0", DataType::Int32, false),
        test_col("c1", DataType::String, false),
        test_col("c2", DataType::Int64, false),
        test_col("c3", DataType::Bool, false),
    ])
    .unwrap();

    let opts = SegmentOptions::new().with_rows_per_block(4);
    let mut rows = Vec::new();
    for i in 0..12 {
        rows.push(Row::new(vec![
            Value::Int32(i),
            Value::String(format!("str_{i}")),
            Value::Int64((i as i64) * 100),
            Value::Bool(i % 2 == 0),
        ]));
    }

    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // Request with reversed/arbitrary projection order: [3, 0, 2]
    let req = ScanRequest::new(
        vec![3, 0, 2],
        Some(Predicate::Gt {
            column: 0,
            value: Value::Int32(2),
        }),
    );
    let res = reader.scan(&req).unwrap();

    let (ref_ids, _) = reference_scan(&schema, &rows, &req);
    let mut global_ids = Vec::new();

    for batch in &res.batches {
        assert_eq!(batch.columns.len(), 3);
        // Column 0 must be DataType::Bool (schema col 3)
        assert_eq!(batch.columns[0].data_type(), DataType::Bool);
        // Column 1 must be DataType::Int32 (schema col 0)
        assert_eq!(batch.columns[1].data_type(), DataType::Int32);
        // Column 2 must be DataType::Int64 (schema col 2)
        assert_eq!(batch.columns[2].data_type(), DataType::Int64);

        for &r in &batch.row_ids {
            global_ids.push((batch.row_start + r as u64) as usize);
        }
    }

    assert_eq!(global_ids, ref_ids);
    verify_scan_against_reference(&reader, &rows, &req, &res);
}

#[test]
fn test_empty_projection_filter_only_scan() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty_proj.col");
    let schema = Schema::new(vec![
        test_col("c0", DataType::Int32, false),
        test_col("c1", DataType::String, true),
    ])
    .unwrap();

    let opts = SegmentOptions::new().with_rows_per_block(5);
    let mut rows = Vec::new();
    for i in 0..15 {
        rows.push(Row::new(vec![
            Value::Int32(i),
            Value::String(format!("s_{i}")),
        ]));
    }

    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // Filter matching rows 11, 12, 13, 14 (all in block 2)
    let req = ScanRequest::new(
        vec![],
        Some(Predicate::Gte {
            column: 0,
            value: Value::Int32(11),
        }),
    );
    let res = reader.scan(&req).unwrap();

    // Blocks 0 and 1 are skipped by zone-map:
    // Block 0: max 4 < 11
    // Block 1: max 9 < 11
    assert_eq!(res.stats.candidate_blocks, 3);
    assert_eq!(res.stats.skipped_blocks, 2);
    assert_eq!(res.stats.decoded_blocks, 1);
    assert_eq!(res.stats.returned_rows, 4);

    assert_eq!(res.batches.len(), 1);
    let batch = &res.batches[0];
    assert_eq!(batch.row_start, 10);
    assert_eq!(batch.row_ids, vec![1, 2, 3, 4]);
    assert_eq!(batch.num_columns(), 0);
    assert_eq!(batch.num_rows(), 4);
}

#[test]
fn test_predicate_column_reuse_when_projected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("pred_reuse.col");
    let schema = Schema::new(vec![
        test_col("c0", DataType::Int32, false),
        test_col("c1", DataType::String, false),
    ])
    .unwrap();

    let opts = SegmentOptions::new().with_rows_per_block(4);
    let mut rows = Vec::new();
    for i in 0..8 {
        rows.push(Row::new(vec![
            Value::Int32(i * 10),
            Value::String(format!("val_{i}")),
        ]));
    }

    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // Predicate on c0, and c0 is in projection: [0, 1]
    let req1 = ScanRequest::new(
        vec![0, 1],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Int32(30),
        }),
    );
    let res1 = reader.scan(&req1).unwrap();
    assert_eq!(res1.total_rows(), 1);
    assert_eq!(res1.batches[0].columns[0].get(0), Some(Value::Int32(30)));
    assert_eq!(
        res1.batches[0].columns[1].get(0),
        Some(Value::String("val_3".into()))
    );

    // Predicate on c0, but reversed projection order: [1, 0]
    let req2 = ScanRequest::new(
        vec![1, 0],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Int32(30),
        }),
    );
    let res2 = reader.scan(&req2).unwrap();
    assert_eq!(res2.total_rows(), 1);
    assert_eq!(
        res2.batches[0].columns[0].get(0),
        Some(Value::String("val_3".into()))
    );
    assert_eq!(res2.batches[0].columns[1].get(0), Some(Value::Int32(30)));
}

#[test]
fn test_invalid_requests_and_type_index_failures() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invalid_reqs.col");
    let schema = Schema::new(vec![
        test_col("c0", DataType::Int32, false),
        test_col("c1", DataType::String, true),
    ])
    .unwrap();

    let opts = SegmentOptions::new().with_rows_per_block(4);
    let rows = vec![Row::new(vec![Value::Int32(1), Value::String("a".into())])];
    SegmentWriter::write(&path, &schema, rows, &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // 1. Out of bounds projection index
    let req_oob_proj = ScanRequest::new(vec![0, 5], None);
    let err = reader.scan(&req_oob_proj).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // 2. Duplicate projection index
    let req_dup_proj = ScanRequest::new(vec![0, 1, 0], None);
    let err = reader.scan(&req_dup_proj).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // 3. Out of bounds predicate index
    let req_oob_pred = ScanRequest::new(vec![0], Some(Predicate::IsNull { column: 99 }));
    let err = reader.scan(&req_oob_pred).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // 4. Literal type mismatch in predicate
    let req_type_mismatch = ScanRequest::new(
        vec![0],
        Some(Predicate::Eq {
            column: 0, // Int32
            value: Value::String("not_int".into()),
        }),
    );
    let err = reader.scan(&req_type_mismatch).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));

    // 5. NULL literal in comparison predicate
    let req_null_lit = ScanRequest::new(
        vec![0],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Null,
        }),
    );
    let err = reader.scan(&req_null_lit).unwrap_err();
    assert!(matches!(err, HtapError::InvalidArgument(_)));
}

#[test]
fn test_corrupt_nonmatching_block_skipped_corrupt_matching_block_errors() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("corruption_selective.col");
    let schema = Schema::new(vec![test_col("c0", DataType::Int32, false)]).unwrap();

    // 2 blocks (5 rows each):
    // Block 0: 0..5 (min 0, max 4)
    // Block 1: 100..105 (min 100, max 104)
    let opts = SegmentOptions::new().with_rows_per_block(5);
    let mut rows = Vec::new();
    for i in 0..5 {
        rows.push(Row::new(vec![Value::Int32(i)]));
    }
    for i in 100..105 {
        rows.push(Row::new(vec![Value::Int32(i)]));
    }

    SegmentWriter::write(&path, &schema, rows, &opts).unwrap();

    let reader_pre = SegmentReader::open(&path).unwrap();
    let b0_meta = reader_pre.block_meta(0, 0).unwrap().clone();
    let b1_meta = reader_pre.block_meta(0, 1).unwrap().clone();
    drop(reader_pre);

    // Deliberately corrupt Block 0 payload on disk:
    // Flip a byte in the stored body of Block 0
    let b0_body_byte = b0_meta.offset + FRAME_HEADER_LEN as u64 + 1;
    flip_byte_at(&path, b0_body_byte);

    // Open reader (footer and header are intact, so open succeeds)
    let reader = SegmentReader::open(&path).unwrap();

    // Scan targeting Block 1: Eq(102)
    // Block 0 has min 0, max 4 -> skipped via zone-map!
    // Block 0's corrupted payload MUST NOT be read.
    let req_target_b1 = ScanRequest::new(
        vec![0],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Int32(102),
        }),
    );
    let res = reader.scan(&req_target_b1).unwrap();
    assert_eq!(res.stats.candidate_blocks, 2);
    assert_eq!(res.stats.skipped_blocks, 1);
    assert_eq!(res.stats.decoded_blocks, 1);
    assert_eq!(res.stats.returned_rows, 1);
    assert_eq!(res.batches.len(), 1);
    assert_eq!(res.batches[0].columns[0].get(0), Some(Value::Int32(102)));

    // Scan targeting Block 0: Eq(2)
    // Block 0 is NOT skipped by zone-map (min 0 <= 2 <= max 4).
    // Block 0 payload is read and its CRC mismatch is detected!
    let req_target_b0 = ScanRequest::new(
        vec![0],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Int32(2),
        }),
    );
    let err = reader.scan(&req_target_b0).unwrap_err();
    assert!(
        matches!(err, HtapError::Corruption(_)),
        "corrupted decoded block must return Corruption, got {err:?}"
    );

    // Now corrupt Block 1 as well and verify it also errors when matching
    let b1_body_byte = b1_meta.offset + FRAME_HEADER_LEN as u64 + 1;
    flip_byte_at(&path, b1_body_byte);

    let err_b1 = reader.scan(&req_target_b1).unwrap_err();
    assert!(
        matches!(err_b1, HtapError::Corruption(_)),
        "corrupted matching block 1 must return Corruption, got {err_b1:?}"
    );
}

#[test]
fn test_no_predicate_scan_decodes_all_blocks_and_returns_all_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("no_predicate.col");
    let schema = Schema::new(vec![
        test_col("c0", DataType::Int32, false),
        test_col("c1", DataType::String, true),
    ])
    .unwrap();

    let opts = SegmentOptions::new().with_rows_per_block(4);
    let mut rows = Vec::new();
    for i in 0..13 {
        rows.push(Row::new(vec![
            Value::Int32(i),
            if i % 3 == 0 {
                Value::Null
            } else {
                Value::String(format!("item_{i}"))
            },
        ]));
    }

    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();
    assert_eq!(reader.block_count(), 4); // 4 + 4 + 4 + 1

    // 1. No-predicate scan with full projection
    let req_all = ScanRequest::new(vec![0, 1], None);
    let res_all = reader.scan(&req_all).unwrap();

    assert_eq!(res_all.stats.candidate_blocks, 4);
    assert_eq!(res_all.stats.skipped_blocks, 0);
    assert_eq!(res_all.stats.decoded_blocks, 4);
    assert_eq!(res_all.stats.returned_rows, 13);
    assert_eq!(res_all.batches.len(), 4);

    verify_scan_against_reference(&reader, &rows, &req_all, &res_all);

    // 2. No-predicate scan with empty projection
    let req_empty = ScanRequest::new(vec![], None);
    let res_empty = reader.scan(&req_empty).unwrap();

    assert_eq!(res_empty.stats.candidate_blocks, 4);
    assert_eq!(res_empty.stats.skipped_blocks, 0);
    assert_eq!(res_empty.stats.decoded_blocks, 4);
    assert_eq!(res_empty.stats.returned_rows, 13);
    assert_eq!(res_empty.batches.len(), 4);

    for batch in &res_empty.batches {
        assert_eq!(batch.num_columns(), 0);
        assert!(!batch.is_empty());
    }
}

#[test]
fn test_float_comparisons_preserve_value_total_ordering() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("float_total_cmp.col");
    let schema = Schema::new(vec![test_col("c_f64", DataType::Float64, false)]).unwrap();

    let opts = SegmentOptions::new().with_rows_per_block(4);
    // Value total ordering: -f64::INFINITY < -0.0 < 0.0 < 1.0 < f64::INFINITY < f64::NAN
    let rows = vec![
        Row::new(vec![Value::Float64(-f64::INFINITY)]),
        Row::new(vec![Value::Float64(-0.0)]),
        Row::new(vec![Value::Float64(0.0)]),
        Row::new(vec![Value::Float64(1.0)]),
        Row::new(vec![Value::Float64(f64::INFINITY)]),
        Row::new(vec![Value::Float64(f64::NAN)]),
    ];

    SegmentWriter::write(&path, &schema, rows.clone(), &opts).unwrap();
    let reader = SegmentReader::open(&path).unwrap();

    // 1. Lt(0.0) should match -inf and -0.0 (since -0.0 < 0.0 in total_cmp)
    let req_lt = ScanRequest::new(
        vec![0],
        Some(Predicate::Lt {
            column: 0,
            value: Value::Float64(0.0),
        }),
    );
    let res_lt = reader.scan(&req_lt).unwrap();
    assert_eq!(res_lt.total_rows(), 2);
    assert_eq!(
        res_lt.batches[0].columns[0].get(0),
        Some(Value::Float64(-f64::INFINITY))
    );
    assert_eq!(
        res_lt.batches[0].columns[0].get(1),
        Some(Value::Float64(-0.0))
    );

    // 2. Gt(1.0) should match +inf and NaN
    let req_gt = ScanRequest::new(
        vec![0],
        Some(Predicate::Gt {
            column: 0,
            value: Value::Float64(1.0),
        }),
    );
    let res_gt = reader.scan(&req_gt).unwrap();
    assert_eq!(res_gt.total_rows(), 2);
    let vals: Vec<Value> = res_gt
        .batches
        .iter()
        .flat_map(|b| (0..b.num_rows()).map(|r| b.columns[0].get(r).unwrap()))
        .collect();
    assert_eq!(vals[0], Value::Float64(f64::INFINITY));
    assert!(matches!(vals[1], Value::Float64(f) if f.is_nan()));

    // 3. Eq(NAN) should match NAN
    let req_nan = ScanRequest::new(
        vec![0],
        Some(Predicate::Eq {
            column: 0,
            value: Value::Float64(f64::NAN),
        }),
    );
    let res_nan = reader.scan(&req_nan).unwrap();
    assert_eq!(res_nan.total_rows(), 1);
    let val = res_nan.batches[0].columns[0].get(0).unwrap();
    assert!(matches!(val, Value::Float64(f) if f.is_nan()));
}
