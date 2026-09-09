//! Columnar storage engine for the HTAP database.
//!
//! Provides columnar segment layout, encoding, compression, zone maps,
//! and vectorized scanning primitives for analytical workloads.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod encoding;
pub mod scan;
pub mod segment;
pub mod types;

pub use scan::{can_block_contain_matches, execute_scan, filter_column_vector};
pub use segment::{
    BlockMeta, SegmentMetadata, SegmentReader, SegmentWriter, FORMAT_VERSION, FRAME_HEADER_LEN,
    HEADER_LEN, HEADER_MAGIC, TRAILER_LEN, TRAILER_MAGIC,
};
pub use types::{
    validate_row, validate_segment_schema, validate_value, ColumnEncoding, ColumnVector, Predicate,
    RecordBatch, ScanRequest, ScanResult, ScanStats, SegmentOptions, DEFAULT_ROWS_PER_BLOCK,
    DEFAULT_ZSTD_LEVEL, MAX_BLOCK_ROWS, MAX_BLOCK_STORED_BYTES, MAX_BLOCK_UNCOMPRESSED_BYTES,
    MAX_FOOTER_BYTES, MAX_SEGMENT_BLOCKS, MAX_SEGMENT_COLUMNS, MAX_VALUE_BYTES, MAX_ZSTD_LEVEL,
    MIN_ZSTD_LEVEL,
};
