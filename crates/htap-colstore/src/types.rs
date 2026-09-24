//! Data types and structures for the columnar storage engine.
//!
//! Defines segment configuration options, columnar vectors, record batches,
//! scan specifications, filter predicates, and validation helpers.

use htap_common::types::check_decimal_precision;
use htap_common::{ColumnDef, DataType, HtapError, Result, Row, Schema, Value};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Default number of rows packed into each columnar block.
pub const DEFAULT_ROWS_PER_BLOCK: usize = 1024;

/// Default compression level for zstd block compression.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Minimum supported zstd compression level.
pub const MIN_ZSTD_LEVEL: i32 = -7;

/// Maximum supported zstd compression level.
pub const MAX_ZSTD_LEVEL: i32 = 22;

/// Maximum number of rows allowed in a single block.
pub const MAX_BLOCK_ROWS: usize = 65_536;

/// Maximum uncompressed bytes for a single block payload (64 MiB).
pub const MAX_BLOCK_UNCOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Maximum stored (compressed or raw) bytes for a single block (64 MiB).
pub const MAX_BLOCK_STORED_BYTES: usize = 64 * 1024 * 1024;

/// Maximum size in bytes of a serialized segment footer (64 MiB).
pub const MAX_FOOTER_BYTES: usize = 64 * 1024 * 1024;

/// Maximum number of columns permitted in a columnar segment schema.
pub const MAX_SEGMENT_COLUMNS: usize = 1_024;

/// Maximum number of blocks in a single segment file.
pub const MAX_SEGMENT_BLOCKS: usize = 1_000_000;

/// Maximum size in bytes of an individual string or byte literal/value (16 MiB).
pub const MAX_VALUE_BYTES: usize = 16 * 1024 * 1024;

/// Options controlling the creation and formatting of a columnar segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentOptions {
    /// Number of rows per block.
    pub rows_per_block: usize,
    /// zstd compression level (-7 to 22).
    pub zstd_level: i32,
}

impl SegmentOptions {
    /// Creates a new `SegmentOptions` instance with default settings.
    pub fn new() -> Self {
        Self {
            rows_per_block: DEFAULT_ROWS_PER_BLOCK,
            zstd_level: DEFAULT_ZSTD_LEVEL,
        }
    }

    /// Sets the target number of rows per block.
    #[must_use]
    pub fn with_rows_per_block(mut self, rows: usize) -> Self {
        self.rows_per_block = rows;
        self
    }

    /// Sets the zstd compression level.
    #[must_use]
    pub fn with_zstd_level(mut self, level: i32) -> Self {
        self.zstd_level = level;
        self
    }

    /// Validates that configuration options fall within acceptable limits.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if `rows_per_block` is 0 or exceeds
    /// [`MAX_BLOCK_ROWS`], or if `zstd_level` is outside `-7..=22`.
    pub fn validate(&self) -> Result<()> {
        if self.rows_per_block == 0 || self.rows_per_block > MAX_BLOCK_ROWS {
            return Err(HtapError::InvalidArgument(format!(
                "rows_per_block must be between 1 and {MAX_BLOCK_ROWS}, got {}",
                self.rows_per_block
            )));
        }
        if !(MIN_ZSTD_LEVEL..=MAX_ZSTD_LEVEL).contains(&self.zstd_level) {
            return Err(HtapError::InvalidArgument(format!(
                "zstd_level must be between {MIN_ZSTD_LEVEL} and {MAX_ZSTD_LEVEL}, got {}",
                self.zstd_level
            )));
        }
        Ok(())
    }
}

impl Default for SegmentOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A filter predicate for pushdown into columnar block scans and zone maps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    /// Column equals value.
    Eq {
        /// Zero-based column index in the segment schema.
        column: usize,
        /// Comparison literal (must be non-NULL).
        value: Value,
    },
    /// Column is less than value.
    Lt {
        /// Zero-based column index in the segment schema.
        column: usize,
        /// Comparison literal (must be non-NULL).
        value: Value,
    },
    /// Column is less than or equal to value.
    Lte {
        /// Zero-based column index in the segment schema.
        column: usize,
        /// Comparison literal (must be non-NULL).
        value: Value,
    },
    /// Column is greater than value.
    Gt {
        /// Zero-based column index in the segment schema.
        column: usize,
        /// Comparison literal (must be non-NULL).
        value: Value,
    },
    /// Column is greater than or equal to value.
    Gte {
        /// Zero-based column index in the segment schema.
        column: usize,
        /// Comparison literal (must be non-NULL).
        value: Value,
    },
    /// Column is NULL.
    IsNull {
        /// Zero-based column index in the segment schema.
        column: usize,
    },
    /// Column is NOT NULL.
    IsNotNull {
        /// Zero-based column index in the segment schema.
        column: usize,
    },
}

impl Predicate {
    /// Returns the target column index of this predicate.
    pub fn column(&self) -> usize {
        match self {
            Predicate::Eq { column, .. }
            | Predicate::Lt { column, .. }
            | Predicate::Lte { column, .. }
            | Predicate::Gt { column, .. }
            | Predicate::Gte { column, .. }
            | Predicate::IsNull { column }
            | Predicate::IsNotNull { column } => *column,
        }
    }

    /// Validates the predicate against the given schema.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if:
    /// - The predicate column index is out of bounds.
    /// - A comparison literal is `Value::Null`.
    /// - A comparison literal's data type does not match the column definition.
    /// - A string or bytes literal exceeds [`MAX_VALUE_BYTES`].
    pub fn validate(&self, schema: &Schema) -> Result<()> {
        validate_segment_schema(schema)?;
        let col_idx = self.column();
        let col_def = schema.column(col_idx).ok_or_else(|| {
            HtapError::InvalidArgument(format!(
                "predicate column index {col_idx} out of range (schema has {} columns)",
                schema.len()
            ))
        })?;

        match self {
            Predicate::IsNull { .. } | Predicate::IsNotNull { .. } => Ok(()),
            Predicate::Eq { value, .. }
            | Predicate::Lt { value, .. }
            | Predicate::Lte { value, .. }
            | Predicate::Gt { value, .. }
            | Predicate::Gte { value, .. } => {
                if value.is_null() {
                    return Err(HtapError::InvalidArgument(
                        "comparison predicate literal must be non-NULL; use IsNull or IsNotNull instead".to_string(),
                    ));
                }
                let val_dt = value
                    .data_type()
                    .expect("non-null value has an associated DataType");
                if val_dt != col_def.data_type {
                    return Err(HtapError::InvalidArgument(format!(
                        "predicate literal type mismatch for column '{}': expected {}, got {}",
                        col_def.name, col_def.data_type, val_dt
                    )));
                }
                if let Value::String(s) = value {
                    if s.len() > MAX_VALUE_BYTES {
                        return Err(HtapError::InvalidArgument(format!(
                            "predicate literal string byte length {} exceeds MAX_VALUE_BYTES {}",
                            s.len(),
                            MAX_VALUE_BYTES
                        )));
                    }
                }
                if let Value::Bytes(b) = value {
                    if b.len() > MAX_VALUE_BYTES {
                        return Err(HtapError::InvalidArgument(format!(
                            "predicate literal bytes length {} exceeds MAX_VALUE_BYTES {}",
                            b.len(),
                            MAX_VALUE_BYTES
                        )));
                    }
                }
                Ok(())
            }
        }
    }

    /// Evaluates the predicate against a single [`Value`] using SQL 3-valued filter semantics.
    ///
    /// Comparison operations with NULL return `false`. Cross-type comparisons return `false`.
    pub fn matches(&self, value: &Value) -> bool {
        match self {
            Predicate::IsNull { .. } => value.is_null(),
            Predicate::IsNotNull { .. } => !value.is_null(),
            Predicate::Eq { value: lit, .. } => {
                if value.is_null() || lit.is_null() || value.data_type() != lit.data_type() {
                    false
                } else {
                    value == lit
                }
            }
            Predicate::Lt { value: lit, .. } => {
                if value.is_null() || lit.is_null() || value.data_type() != lit.data_type() {
                    false
                } else {
                    value < lit
                }
            }
            Predicate::Lte { value: lit, .. } => {
                if value.is_null() || lit.is_null() || value.data_type() != lit.data_type() {
                    false
                } else {
                    value <= lit
                }
            }
            Predicate::Gt { value: lit, .. } => {
                if value.is_null() || lit.is_null() || value.data_type() != lit.data_type() {
                    false
                } else {
                    value > lit
                }
            }
            Predicate::Gte { value: lit, .. } => {
                if value.is_null() || lit.is_null() || value.data_type() != lit.data_type() {
                    false
                } else {
                    value >= lit
                }
            }
        }
    }
}

/// Specification for a columnar scan operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanRequest {
    /// Zero-based column indices to include in the scan projection.
    pub projection: Vec<usize>,
    /// Optional pushdown filter predicate.
    pub predicate: Option<Predicate>,
}

impl ScanRequest {
    /// Creates a new `ScanRequest`.
    pub fn new(projection: Vec<usize>, predicate: Option<Predicate>) -> Self {
        Self {
            projection,
            predicate,
        }
    }

    /// Validates the scan request against the table schema.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if:
    /// - The schema itself is invalid.
    /// - Any projection column index is out of bounds.
    /// - Any projection column index appears more than once.
    /// - The predicate fails validation.
    pub fn validate(&self, schema: &Schema) -> Result<()> {
        validate_segment_schema(schema)?;
        let mut seen = HashSet::with_capacity(self.projection.len());
        for &col_idx in &self.projection {
            if col_idx >= schema.len() {
                return Err(HtapError::InvalidArgument(format!(
                    "projection column index {col_idx} out of range (schema has {} columns)",
                    schema.len()
                )));
            }
            if !seen.insert(col_idx) {
                return Err(HtapError::InvalidArgument(format!(
                    "duplicate projection column index: {col_idx}"
                )));
            }
        }

        if let Some(predicate) = &self.predicate {
            predicate.validate(schema)?;
        }

        Ok(())
    }
}

/// Execution counters and statistics for a columnar segment scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScanStats {
    /// Number of candidate blocks inspected in metadata.
    pub candidate_blocks: usize,
    /// Number of blocks skipped via zone map / metadata pruning.
    pub skipped_blocks: usize,
    /// Number of blocks decompressed and decoded.
    pub decoded_blocks: usize,
    /// Total number of rows returned by the scan.
    pub returned_rows: usize,
}

/// Physical encoding used for a column block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnEncoding {
    /// Plain encoding (values stored consecutively).
    Plain,
    /// Dictionary encoding (dictionary table plus index references).
    Dictionary,
}

/// Strongly typed columnar vector holding a chunk of values and nullability flags.
///
/// Invariant: `values.len()` must always match `validity.len()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColumnVector {
    /// Boolean column vector.
    Bool {
        /// Element values.
        values: Vec<bool>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// 32-bit signed integer column vector.
    Int32 {
        /// Element values.
        values: Vec<i32>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// 64-bit signed integer column vector.
    Int64 {
        /// Element values.
        values: Vec<i64>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// IEEE-754 double precision float column vector.
    Float64 {
        /// Element values.
        values: Vec<f64>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// UTF-8 string column vector.
    String {
        /// Element values.
        values: Vec<String>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// Opaque byte slice column vector.
    Bytes {
        /// Element values.
        values: Vec<Vec<u8>>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// Timestamp column vector (microseconds since Unix epoch).
    Timestamp {
        /// Element values.
        values: Vec<i64>,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
    /// Fixed-point decimal column vector.
    Decimal {
        /// Unscaled element values.
        values: Vec<i64>,
        /// Declared decimal precision.
        precision: u8,
        /// Declared decimal scale.
        scale: u8,
        /// Validity mask (`true` = non-null, `false` = null).
        validity: Vec<bool>,
    },
}

impl ColumnVector {
    /// Returns the number of rows in this column vector.
    ///
    /// # Panics
    /// Panics if the vector's `values` and `validity` lengths do not match.
    pub fn len(&self) -> usize {
        let (v_len, val_len) = self.lengths();
        assert_eq!(
            v_len, val_len,
            "ColumnVector values length ({v_len}) must equal validity length ({val_len})"
        );
        v_len
    }

    /// Returns `true` if this column vector contains 0 rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the logical [`DataType`] represented by this vector variant.
    pub fn data_type(&self) -> DataType {
        match self {
            ColumnVector::Bool { .. } => DataType::Bool,
            ColumnVector::Int32 { .. } => DataType::Int32,
            ColumnVector::Int64 { .. } => DataType::Int64,
            ColumnVector::Float64 { .. } => DataType::Float64,
            ColumnVector::String { .. } => DataType::String,
            ColumnVector::Bytes { .. } => DataType::Bytes,
            ColumnVector::Timestamp { .. } => DataType::Timestamp,
            ColumnVector::Decimal {
                precision, scale, ..
            } => DataType::Decimal {
                precision: *precision,
                scale: *scale,
            },
        }
    }

    /// Returns the validity mask slice, where `true` indicates non-null.
    ///
    /// # Panics
    /// Panics if the vector's `values` and `validity` lengths do not match.
    pub fn validity(&self) -> &[bool] {
        let (v_len, mask) = match self {
            ColumnVector::Bool { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::Int32 { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::Int64 { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::Float64 { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::String { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::Bytes { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::Timestamp { values, validity } => (values.len(), validity.as_slice()),
            ColumnVector::Decimal {
                values, validity, ..
            } => (values.len(), validity.as_slice()),
        };
        assert_eq!(
            v_len,
            mask.len(),
            "ColumnVector values length ({v_len}) must equal validity length ({})",
            mask.len()
        );
        mask
    }

    /// Returns the count of valid (non-null) values in this vector.
    pub fn selected_values(&self) -> usize {
        self.validity().iter().filter(|&&v| v).count()
    }

    /// Returns the [`Value`] at row index `idx`, or `None` if out of bounds.
    ///
    /// If the validity bit at `idx` is `false`, returns `Some(Value::Null)`.
    pub fn get(&self, idx: usize) -> Option<Value> {
        if idx >= self.len() {
            return None;
        }
        if !self.validity()[idx] {
            return Some(Value::Null);
        }
        match self {
            ColumnVector::Bool { values, .. } => Some(Value::Bool(values[idx])),
            ColumnVector::Int32 { values, .. } => Some(Value::Int32(values[idx])),
            ColumnVector::Int64 { values, .. } => Some(Value::Int64(values[idx])),
            ColumnVector::Float64 { values, .. } => Some(Value::Float64(values[idx])),
            ColumnVector::String { values, .. } => Some(Value::String(values[idx].clone())),
            ColumnVector::Bytes { values, .. } => Some(Value::Bytes(values[idx].clone())),
            ColumnVector::Timestamp { values, .. } => Some(Value::Timestamp(values[idx])),
            ColumnVector::Decimal {
                values,
                precision,
                scale,
                ..
            } => Some(Value::Decimal {
                value: values[idx],
                precision: *precision,
                scale: *scale,
            }),
        }
    }

    /// Validates the internal invariants of the column vector.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if `values.len() != validity.len()`
    /// or if any string/byte element exceeds [`MAX_VALUE_BYTES`].
    pub fn validate(&self) -> Result<()> {
        let (v_len, val_len) = self.lengths();
        if v_len != val_len {
            return Err(HtapError::InvalidArgument(format!(
                "ColumnVector values length ({v_len}) does not match validity length ({val_len})"
            )));
        }
        match self {
            ColumnVector::String { values, .. } => {
                for (i, s) in values.iter().enumerate() {
                    if s.len() > MAX_VALUE_BYTES {
                        return Err(HtapError::InvalidArgument(format!(
                            "string element at index {i} length {} exceeds MAX_VALUE_BYTES {MAX_VALUE_BYTES}",
                            s.len()
                        )));
                    }
                }
            }
            ColumnVector::Bytes { values, .. } => {
                for (i, b) in values.iter().enumerate() {
                    if b.len() > MAX_VALUE_BYTES {
                        return Err(HtapError::InvalidArgument(format!(
                            "bytes element at index {i} length {} exceeds MAX_VALUE_BYTES {MAX_VALUE_BYTES}",
                            b.len()
                        )));
                    }
                }
            }
            ColumnVector::Decimal {
                values,
                precision,
                scale,
                validity,
            } => {
                for (&value, &is_valid) in values.iter().zip(validity) {
                    if is_valid {
                        check_decimal_precision(value, *precision, *scale).map_err(|e| {
                            HtapError::Corruption(format!(
                                "invalid decimal value in column vector: {e}"
                            ))
                        })?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn lengths(&self) -> (usize, usize) {
        match self {
            ColumnVector::Bool { values, validity } => (values.len(), validity.len()),
            ColumnVector::Int32 { values, validity } => (values.len(), validity.len()),
            ColumnVector::Int64 { values, validity } => (values.len(), validity.len()),
            ColumnVector::Float64 { values, validity } => (values.len(), validity.len()),
            ColumnVector::String { values, validity } => (values.len(), validity.len()),
            ColumnVector::Bytes { values, validity } => (values.len(), validity.len()),
            ColumnVector::Timestamp { values, validity } => (values.len(), validity.len()),
            ColumnVector::Decimal {
                values, validity, ..
            } => (values.len(), validity.len()),
        }
    }
}

/// A horizontally aligned batch of columnar vectors sharing row identifiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordBatch {
    /// Physical starting row offset within the segment file.
    pub row_start: u64,
    /// Row offsets / IDs relative to `row_start`.
    pub row_ids: Vec<u32>,
    /// Projected column vectors, all having equal length matching `row_ids.len()`.
    pub columns: Vec<ColumnVector>,
}

impl RecordBatch {
    /// Creates a new `RecordBatch`.
    pub fn new(row_start: u64, row_ids: Vec<u32>, columns: Vec<ColumnVector>) -> Self {
        Self {
            row_start,
            row_ids,
            columns,
        }
    }

    /// Returns the number of rows in the batch.
    pub fn num_rows(&self) -> usize {
        self.row_ids.len()
    }

    /// Returns the number of projected columns in the batch.
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    /// Returns `true` if the batch contains zero rows.
    pub fn is_empty(&self) -> bool {
        self.row_ids.is_empty()
    }

    /// Validates the record batch.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if any column vector fails validation
    /// or has a row length differing from `row_ids.len()`.
    pub fn validate(&self) -> Result<()> {
        let expected_rows = self.row_ids.len();
        for (i, col) in self.columns.iter().enumerate() {
            col.validate()?;
            if col.len() != expected_rows {
                return Err(HtapError::InvalidArgument(format!(
                    "column {i} length {} does not match record batch row_ids length {expected_rows}",
                    col.len()
                )));
            }
        }
        Ok(())
    }
}

/// The result of executing a columnar segment scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanResult {
    /// Record batches returned by the scan.
    pub batches: Vec<RecordBatch>,
    /// Aggregate statistics gathered during the scan.
    pub stats: ScanStats,
}

impl ScanResult {
    /// Creates a new `ScanResult`.
    pub fn new(batches: Vec<RecordBatch>, stats: ScanStats) -> Self {
        Self { batches, stats }
    }

    /// Returns the total row count across all record batches.
    pub fn total_rows(&self) -> usize {
        self.batches.iter().map(|b| b.num_rows()).sum()
    }
}

/// Validates that a schema is suitable for columnar segment storage.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if `schema` has 0 columns or more than
/// [`MAX_SEGMENT_COLUMNS`] columns.
pub fn validate_segment_schema(schema: &Schema) -> Result<()> {
    if schema.is_empty() || schema.len() > MAX_SEGMENT_COLUMNS {
        return Err(HtapError::InvalidArgument(format!(
            "segment schema column count must be between 1 and {MAX_SEGMENT_COLUMNS}, got {}",
            schema.len()
        )));
    }
    for column in schema.columns() {
        column.data_type.validate()?;
    }
    Ok(())
}

/// Validates that a row conforms to the segment schema's column count, nullability, and types.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if the row shape, nullability, or types do not match.
pub fn validate_row(schema: &Schema, row: &Row) -> Result<()> {
    validate_segment_schema(schema)?;
    if row.len() != schema.len() {
        return Err(HtapError::InvalidArgument(format!(
            "row length {} does not match schema column count {}",
            row.len(),
            schema.len()
        )));
    }
    for (i, col_def) in schema.columns().iter().enumerate() {
        let val = row
            .get(i)
            .expect("row index guaranteed in bounds by row.len() == schema.len()");
        validate_value(col_def, val)?;
    }
    Ok(())
}

/// Validates that a single value is compatible with the given column definition.
///
/// # Errors
/// Returns [`HtapError::InvalidArgument`] if:
/// - The value is NULL for a non-nullable column.
/// - The value's data type does not match `col_def.data_type`.
/// - A string or byte literal exceeds [`MAX_VALUE_BYTES`].
pub fn validate_value(col_def: &ColumnDef, val: &Value) -> Result<()> {
    match val {
        Value::Null => {
            if !col_def.nullable {
                return Err(HtapError::InvalidArgument(format!(
                    "column '{}' is not nullable, but value is NULL",
                    col_def.name
                )));
            }
        }
        _ => {
            let actual = val
                .data_type()
                .expect("non-null value has an associated DataType");
            if actual != col_def.data_type {
                return Err(HtapError::InvalidArgument(format!(
                    "type mismatch for column '{}': expected {}, got {}",
                    col_def.name, col_def.data_type, actual
                )));
            }
            if let Value::Decimal {
                value,
                precision,
                scale,
            } = val
            {
                check_decimal_precision(*value, *precision, *scale)?;
            }
            if let Value::String(s) = val {
                if s.len() > MAX_VALUE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "string value byte length {} exceeds MAX_VALUE_BYTES {MAX_VALUE_BYTES}",
                        s.len()
                    )));
                }
            }
            if let Value::Bytes(b) = val {
                if b.len() > MAX_VALUE_BYTES {
                    return Err(HtapError::InvalidArgument(format!(
                        "bytes value length {} exceeds MAX_VALUE_BYTES {MAX_VALUE_BYTES}",
                        b.len()
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            test_col("c_bool", DataType::Bool, false),
            test_col("c_int32", DataType::Int32, false),
            test_col("c_int64", DataType::Int64, true),
            test_col("c_float64", DataType::Float64, false),
            test_col("c_string", DataType::String, true),
            test_col("c_bytes", DataType::Bytes, true),
            test_col("c_timestamp", DataType::Timestamp, false),
        ])
        .unwrap()
    }

    #[test]
    fn test_segment_options_defaults_and_builder() {
        let opts = SegmentOptions::new();
        assert_eq!(opts.rows_per_block, DEFAULT_ROWS_PER_BLOCK);
        assert_eq!(opts.zstd_level, DEFAULT_ZSTD_LEVEL);
        assert_eq!(opts, SegmentOptions::default());
        assert!(opts.validate().is_ok());

        let modified = opts.with_rows_per_block(4096).with_zstd_level(7);
        assert_eq!(modified.rows_per_block, 4096);
        assert_eq!(modified.zstd_level, 7);
        assert!(modified.validate().is_ok());
    }

    #[test]
    fn test_segment_options_bounds() {
        // Valid edge cases
        assert!(SegmentOptions::new()
            .with_rows_per_block(1)
            .validate()
            .is_ok());
        assert!(SegmentOptions::new()
            .with_rows_per_block(MAX_BLOCK_ROWS)
            .validate()
            .is_ok());
        assert!(SegmentOptions::new()
            .with_zstd_level(MIN_ZSTD_LEVEL)
            .validate()
            .is_ok());
        assert!(SegmentOptions::new()
            .with_zstd_level(MAX_ZSTD_LEVEL)
            .validate()
            .is_ok());

        // Invalid rows_per_block
        let err_0 = SegmentOptions::new()
            .with_rows_per_block(0)
            .validate()
            .unwrap_err();
        assert!(matches!(err_0, HtapError::InvalidArgument(_)));
        let err_overflow = SegmentOptions::new()
            .with_rows_per_block(MAX_BLOCK_ROWS + 1)
            .validate()
            .unwrap_err();
        assert!(matches!(err_overflow, HtapError::InvalidArgument(_)));

        // Invalid zstd_level
        let err_zstd_low = SegmentOptions::new()
            .with_zstd_level(MIN_ZSTD_LEVEL - 1)
            .validate()
            .unwrap_err();
        assert!(matches!(err_zstd_low, HtapError::InvalidArgument(_)));
        let err_zstd_high = SegmentOptions::new()
            .with_zstd_level(MAX_ZSTD_LEVEL + 1)
            .validate()
            .unwrap_err();
        assert!(matches!(err_zstd_high, HtapError::InvalidArgument(_)));
    }

    #[test]
    fn test_segment_schema_validation() {
        let s = full_schema();
        assert!(validate_segment_schema(&s).is_ok());

        // Oversized schema
        let cols = (0..=MAX_SEGMENT_COLUMNS)
            .map(|i| test_col(&format!("c_{i}"), DataType::Int32, true))
            .collect();
        let huge_schema = Schema::new(cols).unwrap();
        let err = validate_segment_schema(&huge_schema).unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    #[test]
    fn test_validate_row_all_shared_data_types() {
        let schema = full_schema();

        let valid_row = Row::new(vec![
            Value::Bool(true),
            Value::Int32(42),
            Value::Int64(100),
            Value::Float64(3.25),
            Value::String("hello".into()),
            Value::Bytes(vec![0xde, 0xad]),
            Value::Timestamp(1_700_000_000),
        ]);
        assert!(validate_row(&schema, &valid_row).is_ok());

        let valid_row_with_nulls = Row::new(vec![
            Value::Bool(false),
            Value::Int32(-1),
            Value::Null, // c_int64 is nullable
            Value::Float64(0.0),
            Value::Null, // c_string is nullable
            Value::Null, // c_bytes is nullable
            Value::Timestamp(0),
        ]);
        assert!(validate_row(&schema, &valid_row_with_nulls).is_ok());
    }

    #[test]
    fn test_validate_row_shape_and_nullability_and_type_mismatches() {
        let schema = full_schema();

        // Row too short
        let short_row = Row::new(vec![Value::Bool(true)]);
        let err_short = validate_row(&schema, &short_row).unwrap_err();
        assert!(matches!(err_short, HtapError::InvalidArgument(_)));

        // Row too long
        let mut long_vals = vec![
            Value::Bool(true),
            Value::Int32(42),
            Value::Int64(100),
            Value::Float64(3.25),
            Value::String("hello".into()),
            Value::Bytes(vec![0xde, 0xad]),
            Value::Timestamp(1_700_000_000),
            Value::Int32(999),
        ];
        let err_long = validate_row(&schema, &Row::new(long_vals.clone())).unwrap_err();
        assert!(matches!(err_long, HtapError::InvalidArgument(_)));

        // Null in non-nullable column (c_bool is index 0, not nullable)
        long_vals.pop();
        long_vals[0] = Value::Null;
        let err_null = validate_row(&schema, &Row::new(long_vals.clone())).unwrap_err();
        assert!(matches!(err_null, HtapError::InvalidArgument(_)));

        // Data type mismatch (c_int32 expects Int32, give Int64)
        long_vals[0] = Value::Bool(true);
        long_vals[1] = Value::Int64(42);
        let err_type = validate_row(&schema, &Row::new(long_vals)).unwrap_err();
        assert!(matches!(err_type, HtapError::InvalidArgument(_)));
    }

    #[test]
    fn test_predicate_validation_and_rejection_rules() {
        let schema = full_schema();

        // Valid predicates
        assert!(Predicate::Eq {
            column: 1,
            value: Value::Int32(10)
        }
        .validate(&schema)
        .is_ok());
        assert!(Predicate::Lt {
            column: 2,
            value: Value::Int64(50)
        }
        .validate(&schema)
        .is_ok());
        assert!(Predicate::Lte {
            column: 3,
            value: Value::Float64(2.0)
        }
        .validate(&schema)
        .is_ok());
        assert!(Predicate::Gt {
            column: 6,
            value: Value::Timestamp(1234)
        }
        .validate(&schema)
        .is_ok());
        assert!(Predicate::Gte {
            column: 4,
            value: Value::String("foo".into())
        }
        .validate(&schema)
        .is_ok());
        assert!(Predicate::IsNull { column: 0 }.validate(&schema).is_ok());
        assert!(Predicate::IsNotNull { column: 0 }.validate(&schema).is_ok());

        // Out of range column index
        let err_range = Predicate::IsNull { column: 99 }
            .validate(&schema)
            .unwrap_err();
        assert!(matches!(err_range, HtapError::InvalidArgument(_)));

        // Comparison literal must be non-NULL
        let err_null_lit = Predicate::Eq {
            column: 2,
            value: Value::Null,
        }
        .validate(&schema)
        .unwrap_err();
        assert!(matches!(err_null_lit, HtapError::InvalidArgument(_)));

        // Comparison literal type mismatch
        let err_type_mismatch = Predicate::Eq {
            column: 1, // c_int32
            value: Value::Int64(42),
        }
        .validate(&schema)
        .unwrap_err();
        assert!(matches!(err_type_mismatch, HtapError::InvalidArgument(_)));
    }

    #[test]
    fn test_predicate_sql_filter_semantics() {
        let eq_pred = Predicate::Eq {
            column: 0,
            value: Value::Int32(10),
        };
        assert_eq!(eq_pred.column(), 0);

        // NULL comparison returns false in SQL filter semantics
        assert!(!eq_pred.matches(&Value::Null));

        // Matching & non-matching values
        assert!(eq_pred.matches(&Value::Int32(10)));
        assert!(!eq_pred.matches(&Value::Int32(11)));

        // Cross-type mismatch returns false
        assert!(!eq_pred.matches(&Value::Int64(10)));

        // Lt, Lte, Gt, Gte
        let lt = Predicate::Lt {
            column: 0,
            value: Value::Int32(10),
        };
        assert!(!lt.matches(&Value::Null));
        assert!(lt.matches(&Value::Int32(9)));
        assert!(!lt.matches(&Value::Int32(10)));

        let lte = Predicate::Lte {
            column: 0,
            value: Value::Int32(10),
        };
        assert!(lte.matches(&Value::Int32(10)));
        assert!(lte.matches(&Value::Int32(9)));
        assert!(!lte.matches(&Value::Int32(11)));

        let gt = Predicate::Gt {
            column: 0,
            value: Value::Int32(10),
        };
        assert!(!gt.matches(&Value::Null));
        assert!(gt.matches(&Value::Int32(11)));
        assert!(!gt.matches(&Value::Int32(10)));

        let gte = Predicate::Gte {
            column: 0,
            value: Value::Int32(10),
        };
        assert!(gte.matches(&Value::Int32(10)));
        assert!(gte.matches(&Value::Int32(11)));
        assert!(!gte.matches(&Value::Int32(9)));

        // IsNull & IsNotNull
        let is_null = Predicate::IsNull { column: 0 };
        assert!(is_null.matches(&Value::Null));
        assert!(!is_null.matches(&Value::Int32(0)));

        let is_not_null = Predicate::IsNotNull { column: 0 };
        assert!(!is_not_null.matches(&Value::Null));
        assert!(is_not_null.matches(&Value::Int32(0)));
    }

    #[test]
    fn test_scan_request_validation() {
        let schema = full_schema();

        // Valid scan requests
        let req_full = ScanRequest::new(vec![0, 1, 2], None);
        assert!(req_full.validate(&schema).is_ok());

        let req_with_pred = ScanRequest::new(
            vec![1, 3],
            Some(Predicate::Eq {
                column: 1,
                value: Value::Int32(5),
            }),
        );
        assert!(req_with_pred.validate(&schema).is_ok());

        // Empty projection is valid (e.g. COUNT(*))
        let req_empty = ScanRequest::new(vec![], None);
        assert!(req_empty.validate(&schema).is_ok());

        // Out-of-bounds projection index
        let req_oob = ScanRequest::new(vec![0, 99], None);
        let err_oob = req_oob.validate(&schema).unwrap_err();
        assert!(matches!(err_oob, HtapError::InvalidArgument(_)));

        // Duplicate projection indices
        let req_dup = ScanRequest::new(vec![0, 1, 0], None);
        let err_dup = req_dup.validate(&schema).unwrap_err();
        assert!(matches!(err_dup, HtapError::InvalidArgument(_)));

        // Invalid predicate inside request
        let req_bad_pred = ScanRequest::new(
            vec![0],
            Some(Predicate::Eq {
                column: 1,
                value: Value::Null, // NULL rejected
            }),
        );
        let err_bad_pred = req_bad_pred.validate(&schema).unwrap_err();
        assert!(matches!(err_bad_pred, HtapError::InvalidArgument(_)));
    }

    #[test]
    fn test_column_vector_methods_and_all_data_types() {
        // Bool
        let cv_bool = ColumnVector::Bool {
            values: vec![true, false, true],
            validity: vec![true, false, true],
        };
        assert_eq!(cv_bool.len(), 3);
        assert!(!cv_bool.is_empty());
        assert_eq!(cv_bool.data_type(), DataType::Bool);
        assert_eq!(cv_bool.selected_values(), 2);
        assert_eq!(cv_bool.validity(), &[true, false, true]);
        assert_eq!(cv_bool.get(0), Some(Value::Bool(true)));
        assert_eq!(cv_bool.get(1), Some(Value::Null));
        assert_eq!(cv_bool.get(2), Some(Value::Bool(true)));
        assert_eq!(cv_bool.get(3), None);
        assert!(cv_bool.validate().is_ok());

        // Int32
        let cv_i32 = ColumnVector::Int32 {
            values: vec![1, 2],
            validity: vec![true, true],
        };
        assert_eq!(cv_i32.len(), 2);
        assert_eq!(cv_i32.data_type(), DataType::Int32);
        assert_eq!(cv_i32.selected_values(), 2);
        assert_eq!(cv_i32.get(0), Some(Value::Int32(1)));

        // Int64
        let cv_i64 = ColumnVector::Int64 {
            values: vec![100],
            validity: vec![true],
        };
        assert_eq!(cv_i64.len(), 1);
        assert_eq!(cv_i64.data_type(), DataType::Int64);
        assert_eq!(cv_i64.get(0), Some(Value::Int64(100)));

        // Float64
        let cv_f64 = ColumnVector::Float64 {
            values: vec![1.5],
            validity: vec![true],
        };
        assert_eq!(cv_f64.len(), 1);
        assert_eq!(cv_f64.data_type(), DataType::Float64);
        assert_eq!(cv_f64.get(0), Some(Value::Float64(1.5)));

        // String
        let cv_str = ColumnVector::String {
            values: vec!["alpha".into(), "beta".into()],
            validity: vec![true, false],
        };
        assert_eq!(cv_str.len(), 2);
        assert_eq!(cv_str.data_type(), DataType::String);
        assert_eq!(cv_str.selected_values(), 1);
        assert_eq!(cv_str.get(0), Some(Value::String("alpha".into())));
        assert_eq!(cv_str.get(1), Some(Value::Null));

        // Bytes
        let cv_bytes = ColumnVector::Bytes {
            values: vec![vec![1, 2, 3]],
            validity: vec![true],
        };
        assert_eq!(cv_bytes.len(), 1);
        assert_eq!(cv_bytes.data_type(), DataType::Bytes);
        assert_eq!(cv_bytes.get(0), Some(Value::Bytes(vec![1, 2, 3])));

        // Timestamp
        let cv_ts = ColumnVector::Timestamp {
            values: vec![9999],
            validity: vec![true],
        };
        assert_eq!(cv_ts.len(), 1);
        assert_eq!(cv_ts.data_type(), DataType::Timestamp);
        assert_eq!(cv_ts.get(0), Some(Value::Timestamp(9999)));
    }

    #[test]
    fn test_column_vector_length_validity_invariants() {
        let invalid_cv = ColumnVector::Int32 {
            values: vec![1, 2, 3],
            validity: vec![true, false], // length 2 vs 3
        };
        let err = invalid_cv.validate().unwrap_err();
        assert!(matches!(err, HtapError::InvalidArgument(_)));
    }

    #[test]
    #[should_panic(expected = "values length (3) must equal validity length (2)")]
    fn test_column_vector_len_panics_on_invariant_violation() {
        let invalid_cv = ColumnVector::Int32 {
            values: vec![1, 2, 3],
            validity: vec![true, false],
        };
        let _ = invalid_cv.len();
    }

    #[test]
    fn test_record_batch_and_scan_result() {
        let batch = RecordBatch::new(
            0,
            vec![0, 1, 2],
            vec![
                ColumnVector::Int32 {
                    values: vec![10, 20, 30],
                    validity: vec![true, true, true],
                },
                ColumnVector::String {
                    values: vec!["a".into(), "b".into(), "c".into()],
                    validity: vec![true, false, true],
                },
            ],
        );

        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 2);
        assert!(!batch.is_empty());
        assert!(batch.validate().is_ok());

        // Invalid record batch: column row count mismatch
        let mismatched_batch = RecordBatch::new(
            0,
            vec![0, 1],
            vec![ColumnVector::Int32 {
                values: vec![1, 2, 3],
                validity: vec![true, true, true],
            }],
        );
        let err_batch = mismatched_batch.validate().unwrap_err();
        assert!(matches!(err_batch, HtapError::InvalidArgument(_)));

        // Scan stats & ScanResult
        let stats = ScanStats {
            candidate_blocks: 5,
            skipped_blocks: 2,
            decoded_blocks: 3,
            returned_rows: 3,
        };
        let scan_result = ScanResult::new(vec![batch.clone()], stats);
        assert_eq!(scan_result.total_rows(), 3);
        assert_eq!(scan_result.stats.skipped_blocks, 2);
    }

    #[test]
    fn test_serde_roundtrips() {
        let opts = SegmentOptions::new()
            .with_rows_per_block(2048)
            .with_zstd_level(5);
        let serialized = serde_json::to_string(&opts).unwrap();
        let deserialized: SegmentOptions = serde_json::from_str(&serialized).unwrap();
        assert_eq!(opts, deserialized);

        let pred = Predicate::Gt {
            column: 2,
            value: Value::Int64(100),
        };
        let s_pred = serde_json::to_string(&pred).unwrap();
        let d_pred: Predicate = serde_json::from_str(&s_pred).unwrap();
        assert_eq!(pred, d_pred);

        let req = ScanRequest::new(vec![0, 2], Some(pred));
        let s_req = serde_json::to_string(&req).unwrap();
        let d_req: ScanRequest = serde_json::from_str(&s_req).unwrap();
        assert_eq!(req, d_req);

        let enc = ColumnEncoding::Dictionary;
        let s_enc = serde_json::to_string(&enc).unwrap();
        let d_enc: ColumnEncoding = serde_json::from_str(&s_enc).unwrap();
        assert_eq!(enc, d_enc);
    }
}
