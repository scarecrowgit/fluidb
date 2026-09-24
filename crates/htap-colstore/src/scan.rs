//! Vectorized columnar segment scan execution.
//!
//! Provides conservative zone-map pushdown filtering, selection vector construction,
//! and selective column decoding for analytical query processing.

use htap_common::{HtapError, Result, Value};

use crate::segment::{BlockMeta, SegmentReader};
use crate::types::{ColumnVector, Predicate, RecordBatch, ScanRequest, ScanResult, ScanStats};

/// Evaluates whether a block can contain rows matching `predicate` using zone map metadata.
///
/// Uses SQL 3-valued filter semantics and conservative pushdown rules:
/// - `Eq(v)`: all-null skips; non-null-only skips if `v < min || v > max`; any null-containing block is retained conservatively.
/// - `Lt(v)` / `Lte(v)`: all-null skips; otherwise skip when `min >= v` / `min > v`.
/// - `Gt(v)` / `Gte(v)`: any block containing NULL is retained; null-free blocks skip when `max <= v` / `max < v`.
/// - `IsNull`: skip when `has_null == false`.
/// - `IsNotNull`: skip when `has_not_null == false`.
///
/// Returns `true` if the block might contain matching rows (must be scanned),
/// or `false` if the block is guaranteed to contain no matching rows (can be pruned).
#[must_use]
pub fn can_block_contain_matches(meta: &BlockMeta, predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Eq { value, .. } => {
            if !meta.has_not_null {
                false
            } else if !meta.has_null {
                let min = meta
                    .min_value
                    .as_ref()
                    .expect("has_not_null implies min_value is present");
                let max = meta
                    .max_value
                    .as_ref()
                    .expect("has_not_null implies max_value is present");
                !(value < min || value > max)
            } else {
                // Any block containing NULL is retained conservatively
                true
            }
        }
        Predicate::Lt { value, .. } => {
            if !meta.has_not_null {
                false
            } else {
                let min = meta
                    .min_value
                    .as_ref()
                    .expect("has_not_null implies min_value is present");
                min < value
            }
        }
        Predicate::Lte { value, .. } => {
            if !meta.has_not_null {
                false
            } else {
                let min = meta
                    .min_value
                    .as_ref()
                    .expect("has_not_null implies min_value is present");
                min <= value
            }
        }
        Predicate::Gt { value, .. } => {
            if meta.has_null {
                // Any block containing NULL is retained
                true
            } else {
                let max = meta
                    .max_value
                    .as_ref()
                    .expect("null-free block implies max_value is present");
                max > value
            }
        }
        Predicate::Gte { value, .. } => {
            if meta.has_null {
                // Any block containing NULL is retained
                true
            } else {
                let max = meta
                    .max_value
                    .as_ref()
                    .expect("null-free block implies max_value is present");
                max >= value
            }
        }
        Predicate::IsNull { .. } => meta.has_null,
        Predicate::IsNotNull { .. } => meta.has_not_null,
    }
}

/// Filters a [`ColumnVector`] by retaining only the rows at the indices given in `selected_rows`.
///
/// # Panics
/// Panics if any index in `selected_rows` is out of bounds for `vector`.
#[must_use]
pub fn filter_column_vector(vector: &ColumnVector, selected_rows: &[u32]) -> ColumnVector {
    let count = selected_rows.len();
    match vector {
        ColumnVector::Bool { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i]);
                new_valids.push(validity[i]);
            }
            ColumnVector::Bool {
                values: new_vals,
                validity: new_valids,
            }
        }
        ColumnVector::Int32 { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i]);
                new_valids.push(validity[i]);
            }
            ColumnVector::Int32 {
                values: new_vals,
                validity: new_valids,
            }
        }
        ColumnVector::Int64 { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i]);
                new_valids.push(validity[i]);
            }
            ColumnVector::Int64 {
                values: new_vals,
                validity: new_valids,
            }
        }
        ColumnVector::Float64 { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i]);
                new_valids.push(validity[i]);
            }
            ColumnVector::Float64 {
                values: new_vals,
                validity: new_valids,
            }
        }
        ColumnVector::Timestamp { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i]);
                new_valids.push(validity[i]);
            }
            ColumnVector::Timestamp {
                values: new_vals,
                validity: new_valids,
            }
        }
        ColumnVector::Decimal {
            values,
            validity,
            precision,
            scale,
        } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i]);
                new_valids.push(validity[i]);
            }
            ColumnVector::Decimal {
                values: new_vals,
                validity: new_valids,
                precision: *precision,
                scale: *scale,
            }
        }
        ColumnVector::String { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i].clone());
                new_valids.push(validity[i]);
            }
            ColumnVector::String {
                values: new_vals,
                validity: new_valids,
            }
        }
        ColumnVector::Bytes { values, validity } => {
            let mut new_vals = Vec::with_capacity(count);
            let mut new_valids = Vec::with_capacity(count);
            for &idx in selected_rows {
                let i = idx as usize;
                new_vals.push(values[i].clone());
                new_valids.push(validity[i]);
            }
            ColumnVector::Bytes {
                values: new_vals,
                validity: new_valids,
            }
        }
    }
}

fn resolve_predicate_matcher<'predicate, 'column>(
    predicate: &'predicate Predicate,
    col: &'column ColumnVector,
) -> Result<Box<dyn Fn(usize) -> bool + 'column>>
where
    'predicate: 'column,
{
    if let Predicate::Eq { value, .. }
    | Predicate::Lt { value, .. }
    | Predicate::Lte { value, .. }
    | Predicate::Gt { value, .. }
    | Predicate::Gte { value, .. } = predicate
    {
        let predicate_type = value.data_type().ok_or_else(|| {
            HtapError::Corruption("comparison predicate contains an untyped NULL value".into())
        })?;
        let column_type = col.data_type();

        if predicate_type != column_type {
            return Err(HtapError::Corruption(format!(
                "predicate value type {predicate_type:?} does not match column vector type {column_type:?}"
            )));
        }
    }

    Ok(Box::new(move |row_idx| {
        predicate_matches_non_null(predicate, col, row_idx)
    }))
}

#[allow(clippy::bool_comparison)]
fn predicate_matches_non_null(predicate: &Predicate, col: &ColumnVector, row_idx: usize) -> bool {
    match (predicate, col) {
        (
            Predicate::Eq {
                value: Value::Bool(lit),
                ..
            },
            ColumnVector::Bool { values, .. },
        ) => values[row_idx] == *lit,
        (
            Predicate::Eq {
                value: Value::Int32(lit),
                ..
            },
            ColumnVector::Int32 { values, .. },
        ) => values[row_idx] == *lit,
        (
            Predicate::Eq {
                value: Value::Int64(lit),
                ..
            },
            ColumnVector::Int64 { values, .. },
        ) => values[row_idx] == *lit,
        (
            Predicate::Eq {
                value: Value::Float64(lit),
                ..
            },
            ColumnVector::Float64 { values, .. },
        ) => values[row_idx].total_cmp(lit).is_eq(),
        (
            Predicate::Eq {
                value: Value::Timestamp(lit),
                ..
            },
            ColumnVector::Timestamp { values, .. },
        ) => values[row_idx] == *lit,
        (
            Predicate::Eq {
                value: Value::Decimal { value: lit, .. },
                ..
            },
            ColumnVector::Decimal { values, .. },
        ) => values[row_idx] == *lit,
        (
            Predicate::Eq {
                value: Value::String(lit),
                ..
            },
            ColumnVector::String { values, .. },
        ) => &values[row_idx] == lit,
        (
            Predicate::Eq {
                value: Value::Bytes(lit),
                ..
            },
            ColumnVector::Bytes { values, .. },
        ) => &values[row_idx] == lit,

        (
            Predicate::Lt {
                value: Value::Bool(lit),
                ..
            },
            ColumnVector::Bool { values, .. },
        ) => values[row_idx] < *lit,
        (
            Predicate::Lt {
                value: Value::Int32(lit),
                ..
            },
            ColumnVector::Int32 { values, .. },
        ) => values[row_idx] < *lit,
        (
            Predicate::Lt {
                value: Value::Int64(lit),
                ..
            },
            ColumnVector::Int64 { values, .. },
        ) => values[row_idx] < *lit,
        (
            Predicate::Lt {
                value: Value::Float64(lit),
                ..
            },
            ColumnVector::Float64 { values, .. },
        ) => values[row_idx].total_cmp(lit).is_lt(),
        (
            Predicate::Lt {
                value: Value::Timestamp(lit),
                ..
            },
            ColumnVector::Timestamp { values, .. },
        ) => values[row_idx] < *lit,
        // Decimal values are raw unscaled integers; validation ensures scales match.
        (
            Predicate::Lt {
                value: Value::Decimal { value: lit, .. },
                ..
            },
            ColumnVector::Decimal { values, .. },
        ) => values[row_idx] < *lit,
        (
            Predicate::Lt {
                value: Value::String(lit),
                ..
            },
            ColumnVector::String { values, .. },
        ) => &values[row_idx] < lit,
        (
            Predicate::Lt {
                value: Value::Bytes(lit),
                ..
            },
            ColumnVector::Bytes { values, .. },
        ) => &values[row_idx] < lit,

        (
            Predicate::Lte {
                value: Value::Bool(lit),
                ..
            },
            ColumnVector::Bool { values, .. },
        ) => values[row_idx] <= *lit,
        (
            Predicate::Lte {
                value: Value::Int32(lit),
                ..
            },
            ColumnVector::Int32 { values, .. },
        ) => values[row_idx] <= *lit,
        (
            Predicate::Lte {
                value: Value::Int64(lit),
                ..
            },
            ColumnVector::Int64 { values, .. },
        ) => values[row_idx] <= *lit,
        (
            Predicate::Lte {
                value: Value::Float64(lit),
                ..
            },
            ColumnVector::Float64 { values, .. },
        ) => values[row_idx].total_cmp(lit).is_le(),
        (
            Predicate::Lte {
                value: Value::Timestamp(lit),
                ..
            },
            ColumnVector::Timestamp { values, .. },
        ) => values[row_idx] <= *lit,
        // Decimal values are raw unscaled integers; validation ensures scales match.
        (
            Predicate::Lte {
                value: Value::Decimal { value: lit, .. },
                ..
            },
            ColumnVector::Decimal { values, .. },
        ) => values[row_idx] <= *lit,
        (
            Predicate::Lte {
                value: Value::String(lit),
                ..
            },
            ColumnVector::String { values, .. },
        ) => &values[row_idx] <= lit,
        (
            Predicate::Lte {
                value: Value::Bytes(lit),
                ..
            },
            ColumnVector::Bytes { values, .. },
        ) => &values[row_idx] <= lit,

        (
            Predicate::Gt {
                value: Value::Bool(lit),
                ..
            },
            ColumnVector::Bool { values, .. },
        ) => values[row_idx] > *lit,
        (
            Predicate::Gt {
                value: Value::Int32(lit),
                ..
            },
            ColumnVector::Int32 { values, .. },
        ) => values[row_idx] > *lit,
        (
            Predicate::Gt {
                value: Value::Int64(lit),
                ..
            },
            ColumnVector::Int64 { values, .. },
        ) => values[row_idx] > *lit,
        (
            Predicate::Gt {
                value: Value::Float64(lit),
                ..
            },
            ColumnVector::Float64 { values, .. },
        ) => values[row_idx].total_cmp(lit).is_gt(),
        (
            Predicate::Gt {
                value: Value::Timestamp(lit),
                ..
            },
            ColumnVector::Timestamp { values, .. },
        ) => values[row_idx] > *lit,
        // Decimal values are raw unscaled integers; validation ensures scales match.
        (
            Predicate::Gt {
                value: Value::Decimal { value: lit, .. },
                ..
            },
            ColumnVector::Decimal { values, .. },
        ) => values[row_idx] > *lit,
        (
            Predicate::Gt {
                value: Value::String(lit),
                ..
            },
            ColumnVector::String { values, .. },
        ) => &values[row_idx] > lit,
        (
            Predicate::Gt {
                value: Value::Bytes(lit),
                ..
            },
            ColumnVector::Bytes { values, .. },
        ) => &values[row_idx] > lit,

        (
            Predicate::Gte {
                value: Value::Bool(lit),
                ..
            },
            ColumnVector::Bool { values, .. },
        ) => values[row_idx] >= *lit,
        (
            Predicate::Gte {
                value: Value::Int32(lit),
                ..
            },
            ColumnVector::Int32 { values, .. },
        ) => values[row_idx] >= *lit,
        (
            Predicate::Gte {
                value: Value::Int64(lit),
                ..
            },
            ColumnVector::Int64 { values, .. },
        ) => values[row_idx] >= *lit,
        (
            Predicate::Gte {
                value: Value::Float64(lit),
                ..
            },
            ColumnVector::Float64 { values, .. },
        ) => values[row_idx].total_cmp(lit).is_ge(),
        (
            Predicate::Gte {
                value: Value::Timestamp(lit),
                ..
            },
            ColumnVector::Timestamp { values, .. },
        ) => values[row_idx] >= *lit,
        // Decimal values are raw unscaled integers; validation ensures scales match.
        (
            Predicate::Gte {
                value: Value::Decimal { value: lit, .. },
                ..
            },
            ColumnVector::Decimal { values, .. },
        ) => values[row_idx] >= *lit,
        (
            Predicate::Gte {
                value: Value::String(lit),
                ..
            },
            ColumnVector::String { values, .. },
        ) => &values[row_idx] >= lit,
        (
            Predicate::Gte {
                value: Value::Bytes(lit),
                ..
            },
            ColumnVector::Bytes { values, .. },
        ) => &values[row_idx] >= lit,

        // Null predicates are evaluated from validity bitmaps before a matcher is constructed;
        // non-match is safe if one reaches this comparison-only matcher.
        (
            Predicate::IsNull { .. } | Predicate::IsNotNull { .. },
            ColumnVector::Bool { .. }
            | ColumnVector::Int32 { .. }
            | ColumnVector::Int64 { .. }
            | ColumnVector::Float64 { .. }
            | ColumnVector::Timestamp { .. }
            | ColumnVector::Decimal { .. }
            | ColumnVector::String { .. }
            | ColumnVector::Bytes { .. },
        ) => false,

        // NULL comparison literals are rejected during request validation and matcher resolution;
        // non-match is safe if a malformed predicate reaches this matcher.
        (
            Predicate::Eq {
                value: Value::Null, ..
            }
            | Predicate::Lt {
                value: Value::Null, ..
            }
            | Predicate::Lte {
                value: Value::Null, ..
            }
            | Predicate::Gt {
                value: Value::Null, ..
            }
            | Predicate::Gte {
                value: Value::Null, ..
            },
            ColumnVector::Bool { .. }
            | ColumnVector::Int32 { .. }
            | ColumnVector::Int64 { .. }
            | ColumnVector::Float64 { .. }
            | ColumnVector::Timestamp { .. }
            | ColumnVector::Decimal { .. }
            | ColumnVector::String { .. }
            | ColumnVector::Bytes { .. },
        ) => false,

        // Correctness is enforced by resolve_predicate_matcher's logical-type comparison,
        // not by compiler exhaustiveness. A non-match is deliberate because a scan-path
        // crash is worse; this is unreachable for current column types. If revisited,
        // narrow this function's input to comparison predicates for compiler enforcement.
        _ => false,
    }
}

/// Executes a scan against a columnar segment reader according to the specification in `request`.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if `request` fails validation against the segment schema.
/// Returns [`HtapError::Corruption`] if any required block payload is corrupted.
/// Returns [`HtapError::Io`] on I/O failure.
pub fn execute_scan(reader: &SegmentReader, request: &ScanRequest) -> Result<ScanResult> {
    request.validate(reader.schema())?;

    let num_blocks = reader.block_count();
    let mut stats = ScanStats::default();
    let mut batches = Vec::new();

    if num_blocks == 0 {
        return Ok(ScanResult::new(batches, stats));
    }

    for b_idx in 0..num_blocks {
        stats.candidate_blocks += 1;

        if let Some(pred) = &request.predicate {
            let meta = reader
                .block_meta(pred.column(), b_idx)
                .expect("block metadata exists for valid column index and block index");
            if !can_block_contain_matches(meta, pred) {
                stats.skipped_blocks += 1;
                continue;
            }
        }

        stats.decoded_blocks += 1;

        let col0_meta = reader
            .block_meta(0, b_idx)
            .expect("block metadata exists for column 0");
        let row_start = col0_meta.row_start;
        let block_row_count = col0_meta.row_count;

        // Determine matching rows
        let (selected_rows, decoded_pred_col) = match &request.predicate {
            None => {
                let all_rows: Vec<u32> = (0..block_row_count).collect();
                (all_rows, None)
            }
            Some(pred) => {
                let pred_col_idx = pred.column();
                let is_null_pred =
                    matches!(pred, Predicate::IsNull { .. } | Predicate::IsNotNull { .. });
                let pred_is_projected = request.projection.contains(&pred_col_idx);

                if is_null_pred && !pred_is_projected {
                    // Validity bitmap alone suffices
                    let validity = reader.read_block_validity(pred_col_idx, b_idx)?;
                    let mut selected = Vec::new();
                    match pred {
                        Predicate::IsNull { .. } => {
                            for (r, &is_valid) in validity.iter().enumerate() {
                                if !is_valid {
                                    selected.push(r as u32);
                                }
                            }
                        }
                        Predicate::IsNotNull { .. } => {
                            for (r, &is_valid) in validity.iter().enumerate() {
                                if is_valid {
                                    selected.push(r as u32);
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                    (selected, None)
                } else {
                    // Must decode predicate column
                    let col_vec = reader.read_block_internal(pred_col_idx, b_idx)?;
                    col_vec.validate()?;
                    let mut selected = Vec::new();
                    let validity = col_vec.validity();
                    match pred {
                        Predicate::IsNull { .. } => {
                            for (r, &is_valid) in validity.iter().enumerate() {
                                if !is_valid {
                                    selected.push(r as u32);
                                }
                            }
                        }
                        Predicate::IsNotNull { .. } => {
                            for (r, &is_valid) in validity.iter().enumerate() {
                                if is_valid {
                                    selected.push(r as u32);
                                }
                            }
                        }
                        _ => {
                            let matcher = resolve_predicate_matcher(pred, &col_vec)?;
                            for (r, &is_valid) in validity.iter().enumerate() {
                                if is_valid && matcher(r) {
                                    selected.push(r as u32);
                                }
                            }
                        }
                    }
                    (selected, Some((pred_col_idx, col_vec)))
                }
            }
        };

        if selected_rows.is_empty() {
            continue;
        }

        stats.returned_rows += selected_rows.len();

        let all_selected = selected_rows.len() == block_row_count as usize;
        let mut batch_columns = Vec::with_capacity(request.projection.len());

        for &proj_col_idx in &request.projection {
            if let Some((col_idx, ref pred_col)) = decoded_pred_col {
                if proj_col_idx == col_idx {
                    let col = if all_selected {
                        pred_col.clone()
                    } else {
                        filter_column_vector(pred_col, &selected_rows)
                    };
                    batch_columns.push(col);
                    continue;
                }
            }

            let full_col = reader.read_block_internal(proj_col_idx, b_idx)?;
            let col = if all_selected {
                full_col
            } else {
                filter_column_vector(&full_col, &selected_rows)
            };
            batch_columns.push(col);
        }

        let batch = RecordBatch::new(row_start, selected_rows, batch_columns);
        batch.validate()?;
        batches.push(batch);
    }

    Ok(ScanResult::new(batches, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_matcher_type_mismatch_is_corruption() {
        let predicate = Predicate::Eq {
            column: 0,
            value: Value::Int32(1),
        };
        let column = ColumnVector::String {
            values: vec!["value".to_owned()],
            validity: vec![true],
        };

        let error = match resolve_predicate_matcher(&predicate, &column) {
            Err(error) => error,
            Ok(_) => panic!("expected corruption error"),
        };

        let HtapError::Corruption(message) = error else {
            panic!("expected corruption error");
        };
        assert!(message.contains("Int32"));
        assert!(message.contains("String"));
    }

    #[test]
    fn predicate_matcher_null_comparison_literal_is_corruption() {
        let predicate = Predicate::Eq {
            column: 0,
            value: Value::Null,
        };
        let column = ColumnVector::Int32 {
            values: vec![1],
            validity: vec![true],
        };

        let error = match resolve_predicate_matcher(&predicate, &column) {
            Err(error) => error,
            Ok(_) => panic!("expected corruption error"),
        };

        assert!(matches!(error, HtapError::Corruption(_)));
    }
}
