//! Deterministic acceptance test for columnar segment zone-map skip pruning (Requirement R1).

use htap_colstore::{Predicate, ScanRequest, SegmentOptions, SegmentReader, SegmentWriter};
use htap_common::{ColumnDef, DataType, Row, Schema, Value};
use tempfile::tempdir;

const NUM_BLOCKS: usize = 100;
const ROWS_PER_BLOCK: usize = 1024;
const TARGET_BLOCK: usize = 73;
const TARGET_ROW_IN_BLOCK: usize = 42;

#[test]
fn test_deterministic_zone_map_skip_acceptance() {
    let dir = tempdir().expect("create temp dir");
    let segment_path = dir.path().join("zone_map_skip_acceptance.col");

    // Fixed schema with an Int64 predicate column and a projected payload column
    let schema = Schema::new(vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: false,
        },
        ColumnDef {
            name: "payload".to_string(),
            data_type: DataType::String,
            nullable: false,
            primary_key: false,
        },
    ])
    .expect("valid schema");

    // Configure exactly 1024 rows per block
    let options = SegmentOptions::new().with_rows_per_block(ROWS_PER_BLOCK);

    // Generate exactly 100 row-aligned blocks with deterministic block-local integer ranges
    let mut rows = Vec::with_capacity(NUM_BLOCKS * ROWS_PER_BLOCK);
    for b in 0..NUM_BLOCKS {
        let block_base = (b as i64) * 10_000;
        for r in 0..ROWS_PER_BLOCK {
            let id = block_base + (r as i64);
            let payload = format!("payload_b{b}_r{r}");
            rows.push(Row::new(vec![Value::Int64(id), Value::String(payload)]));
        }
    }

    assert_eq!(rows.len(), NUM_BLOCKS * ROWS_PER_BLOCK);

    // Write to a fresh tempfile segment
    let meta = SegmentWriter::write(&segment_path, &schema, rows, &options)
        .expect("write segment successfully");
    assert_eq!(meta.row_count, (NUM_BLOCKS * ROWS_PER_BLOCK) as u64);
    assert_eq!(meta.block_count, NUM_BLOCKS);

    // Reopen the fresh segment
    let reader = SegmentReader::open(&segment_path).expect("open segment successfully");
    assert_eq!(reader.block_count(), NUM_BLOCKS);

    // Narrow equality predicate targeting only block 73
    let target_id = (TARGET_BLOCK as i64) * 10_000 + (TARGET_ROW_IN_BLOCK as i64);
    let request = ScanRequest::new(
        vec![1], // Project payload column only
        Some(Predicate::Eq {
            column: 0,
            value: Value::Int64(target_id),
        }),
    );

    // Run reader.scan
    let result = reader.scan(&request).expect("scan succeeds");

    // Assertions mandated by R1 acceptance criteria:
    // 1. stats.candidate_blocks == 100
    assert_eq!(
        result.stats.candidate_blocks, NUM_BLOCKS,
        "candidate_blocks must equal {NUM_BLOCKS}"
    );

    // 2. stats.skipped_blocks >= 90 (designed result should be 99)
    assert!(
        result.stats.skipped_blocks >= 90,
        "skipped_blocks must be >= 90, got {}",
        result.stats.skipped_blocks
    );
    assert_eq!(
        result.stats.skipped_blocks, 99,
        "designed result must skip exactly 99 blocks"
    );

    // 3. skip ratio >= 90%
    let skip_ratio = result.stats.skipped_blocks as f64 / result.stats.candidate_blocks as f64;
    assert!(
        skip_ratio >= 0.90,
        "skip ratio must be >= 90%, got {skip_ratio:.4}"
    );
    assert!(
        (skip_ratio - 0.99).abs() < f64::EPSILON,
        "skip ratio must be exactly 99%, got {skip_ratio:.4}"
    );

    // 4. stats.decoded_blocks <= 10
    assert!(
        result.stats.decoded_blocks <= 10,
        "decoded_blocks must be <= 10, got {}",
        result.stats.decoded_blocks
    );
    assert_eq!(
        result.stats.decoded_blocks, 1,
        "designed result must decode exactly 1 block (block 73)"
    );

    // 5. exact returned row ordinals correspond to block 73
    let expected_ordinal = (TARGET_BLOCK * ROWS_PER_BLOCK + TARGET_ROW_IN_BLOCK) as u64;
    let returned_ordinals: Vec<u64> = result
        .batches
        .iter()
        .flat_map(|batch| {
            batch
                .row_ids
                .iter()
                .map(|&rel_id| batch.row_start + rel_id as u64)
        })
        .collect();

    assert_eq!(
        returned_ordinals,
        vec![expected_ordinal],
        "exact returned row ordinals must match row {TARGET_ROW_IN_BLOCK} of block {TARGET_BLOCK}"
    );
    for &ord in &returned_ordinals {
        let block_idx = (ord / ROWS_PER_BLOCK as u64) as usize;
        assert_eq!(
            block_idx, TARGET_BLOCK,
            "returned row ordinal {ord} must correspond to block {TARGET_BLOCK}"
        );
        let block_start = (TARGET_BLOCK * ROWS_PER_BLOCK) as u64;
        let block_end = ((TARGET_BLOCK + 1) * ROWS_PER_BLOCK) as u64;
        assert!(
            ord >= block_start && ord < block_end,
            "ordinal {ord} out of bounds for block {TARGET_BLOCK} [{block_start}..{block_end})"
        );
    }

    // 6. exact projected values are correct
    assert_eq!(result.batches.len(), 1, "expected exactly one record batch");
    let batch = &result.batches[0];
    assert_eq!(batch.num_rows(), 1, "expected exactly one returned row");
    assert_eq!(
        batch.num_columns(),
        1,
        "expected exactly one projected column"
    );
    let expected_payload = format!("payload_b{TARGET_BLOCK}_r{TARGET_ROW_IN_BLOCK}");
    let actual_payload = batch.columns[0]
        .get(0)
        .expect("projected column value must exist");
    assert_eq!(
        actual_payload,
        Value::String(expected_payload),
        "exact projected payload value must match"
    );
}
