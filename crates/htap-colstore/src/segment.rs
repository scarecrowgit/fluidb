//! Durable columnar segment writer and reader.
//!
//! Provides the immutable on-disk columnar segment implementation for analytical
//! workloads, including checksummed block frames, dictionary encodings, zstd compression,
//! and footer zone maps.
//!
//! # On-Disk Layout (Segment v1)
//!
//! All integers are little-endian.
//!
//! ```text
//! file := header column_block_frame* footer_payload footer_trailer
//!
//! header := magic[8] = "HTAPCOL1"
//!
//! column_block_frame :=
//!   column_index: u32            // 4 bytes
//!   row_start: u64               // 8 bytes
//!   row_count: u32               // 4 bytes
//!   encoding_tag: u8             // 1 byte (0 = Plain, 1 = Dictionary)
//!   null_bitmap_len: u32         // 4 bytes (ceil(row_count / 8))
//!   raw_payload_len: u32         // 4 bytes
//!   stored_payload_len: u32      // 4 bytes
//!   crc32c: u32                  // 4 bytes CRC32-C over stored block body
//!   body: [u8; null_bitmap_len + stored_payload_len]
//!
//! footer payload :=
//!   format_version: u16 = 1      // 2 bytes
//!   schema_len: u32              // 4 bytes
//!   schema_json: [u8; schema_len] // serde_json of Schema
//!   total_rows: u64              // 8 bytes
//!   column_count: u32            // 4 bytes
//!   column_entry*
//!
//! column_entry :=
//!   column_index: u32            // 4 bytes
//!   block_count: u32             // 4 bytes
//!   block_entry*
//!
//! block_entry :=
//!   offset: u64                  // 8 bytes (offset of column_block_frame)
//!   frame_len: u32               // 4 bytes (33 + null_bitmap_len + stored_payload_len)
//!   row_start: u64               // 8 bytes
//!   row_count: u32               // 4 bytes
//!   encoding: u8                 // 1 byte (0 = Plain, 1 = Dictionary)
//!   raw_bytes: u32               // 4 bytes
//!   stored_bytes: u32            // 4 bytes
//!   crc32c: u32                  // 4 bytes
//!   has_null: u8                 // 1 byte (0 = false, 1 = true)
//!   has_not_null: u8             // 1 byte (0 = false, 1 = true)
//!   min_value: typed_value       // present only if has_not_null == 1
//!   max_value: typed_value       // present only if has_not_null == 1
//!
//! footer trailer :=
//!   footer_len: u32              // 4 bytes
//!   footer_crc32c: u32           // 4 bytes (CRC32-C over footer payload only)
//!   magic[4] = "HEND"            // 4 bytes
//! ```

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use htap_common::{ColumnDef, DataType, HtapError, Result, Row, Schema, Value};
use serde::{Deserialize, Serialize};

use crate::encoding::{
    compress_payload, decode_dictionary, decode_null_bitmap, decode_plain, decode_typed_value,
    decompress_payload, encode_dictionary, encode_null_bitmap, encode_plain, encode_typed_value,
};
use crate::types::{
    validate_row, validate_segment_schema, ColumnEncoding, ColumnVector, ScanRequest, ScanResult,
    SegmentOptions, MAX_BLOCK_ROWS, MAX_BLOCK_STORED_BYTES, MAX_BLOCK_UNCOMPRESSED_BYTES,
    MAX_FOOTER_BYTES, MAX_SEGMENT_BLOCKS, MAX_SEGMENT_COLUMNS,
};

/// Header magic identifier for Segment v1 files.
pub const HEADER_MAGIC: &[u8; 8] = b"HTAPCOL1";

/// Trailer magic identifier for Segment v1 files.
pub const TRAILER_MAGIC: &[u8; 4] = b"HEND";

/// Current on-disk columnar segment format version.
pub const FORMAT_VERSION: u16 = 1;

/// Length of the leading file header in bytes.
pub const HEADER_LEN: usize = 8;

/// Length of the fixed footer trailer in bytes (`footer_len: u32` + `footer_crc32c: u32` + `magic: [u8; 4]`).
pub const TRAILER_LEN: usize = 12;

/// Length of the fixed column block frame header in bytes.
pub const FRAME_HEADER_LEN: usize = 33;

/// Public metadata describing an on-disk columnar segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentMetadata {
    /// Path to the segment file.
    pub path: PathBuf,
    /// Total number of rows in the segment.
    pub row_count: u64,
    /// Table schema defining the segment's columns and data types.
    pub schema: Schema,
    /// Number of columns in the segment.
    pub column_count: usize,
    /// Number of row-aligned blocks in the segment.
    pub block_count: usize,
}

/// Metadata and zone map information for a single columnar block frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockMeta {
    /// Starting byte offset of the block frame within the segment file.
    pub offset: u64,
    /// Total byte length of the block frame on disk (header + bitmap + stored payload).
    pub frame_len: u32,
    /// Physical starting row ordinal of this block.
    pub row_start: u64,
    /// Number of rows covered by this block.
    pub row_count: u32,
    /// Physical encoding used for non-null values.
    pub encoding: ColumnEncoding,
    /// Raw uncompressed payload byte size.
    pub raw_bytes: u32,
    /// Stored (compressed or raw) payload byte size.
    pub stored_bytes: u32,
    /// CRC32-C checksum over the stored block body (bitmap + stored payload).
    pub crc32c: u32,
    /// Whether any row in this block is NULL.
    pub has_null: bool,
    /// Whether any row in this block is non-NULL.
    pub has_not_null: bool,
    /// Minimum non-NULL value according to [`Value`] total ordering (`None` if all NULL).
    pub min_value: Option<Value>,
    /// Maximum non-NULL value according to [`Value`] total ordering (`None` if all NULL).
    pub max_value: Option<Value>,
}

/// Writer for durable columnar segment files.
pub struct SegmentWriter;

impl SegmentWriter {
    /// Writes a complete columnar segment file from the given rows and configuration options.
    ///
    /// # Durability
    /// Flushes and fsyncs the file to durable storage before returning. If an error occurs
    /// during writing, any partially written output file is removed.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if options, schema, or rows fail validation.
    /// Returns [`HtapError::Io`] if file I/O fails.
    pub fn write(
        path: &Path,
        schema: &Schema,
        rows: impl IntoIterator<Item = Row>,
        options: &SegmentOptions,
    ) -> Result<SegmentMetadata> {
        validate_segment_schema(schema)?;
        options.validate()?;

        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);

        match Self::write_internal(&mut writer, path, schema, rows, options) {
            Ok(metadata) => {
                writer.flush()?;
                writer.get_ref().sync_all()?;
                Ok(metadata)
            }
            Err(e) => {
                drop(writer);
                let _ = std::fs::remove_file(path);
                Err(e)
            }
        }
    }

    fn write_internal(
        writer: &mut BufWriter<File>,
        path: &Path,
        schema: &Schema,
        rows: impl IntoIterator<Item = Row>,
        options: &SegmentOptions,
    ) -> Result<SegmentMetadata> {
        writer.write_all(HEADER_MAGIC)?;

        let mut current_offset: u64 = HEADER_LEN as u64;
        let mut column_block_metas: Vec<Vec<BlockMeta>> = vec![Vec::new(); schema.len()];
        let mut total_rows: u64 = 0;
        let mut current_chunk: Vec<Row> = Vec::with_capacity(options.rows_per_block);

        for row in rows {
            validate_row(schema, &row)?;
            current_chunk.push(row);
            if current_chunk.len() == options.rows_per_block {
                Self::flush_block(
                    writer,
                    schema,
                    &current_chunk,
                    total_rows,
                    options,
                    &mut current_offset,
                    &mut column_block_metas,
                )?;
                total_rows += current_chunk.len() as u64;
                current_chunk.clear();
            }
        }

        if !current_chunk.is_empty() {
            Self::flush_block(
                writer,
                schema,
                &current_chunk,
                total_rows,
                options,
                &mut current_offset,
                &mut column_block_metas,
            )?;
            total_rows += current_chunk.len() as u64;
            current_chunk.clear();
        }

        let num_row_blocks = if schema.is_empty() || column_block_metas.is_empty() {
            0
        } else {
            column_block_metas[0].len()
        };

        let total_frames: usize = column_block_metas.iter().map(Vec::len).sum();
        if total_frames > MAX_SEGMENT_BLOCKS {
            return Err(HtapError::InvalidArgument(format!(
                "total block frames {total_frames} exceeds MAX_SEGMENT_BLOCKS {MAX_SEGMENT_BLOCKS}"
            )));
        }

        // Build footer payload
        let mut footer_payload = Vec::new();
        footer_payload.extend_from_slice(&FORMAT_VERSION.to_le_bytes());

        let schema_json = serde_json::to_vec(schema)
            .map_err(|e| HtapError::Internal(format!("failed to serialize schema: {e}")))?;
        footer_payload.extend_from_slice(&(schema_json.len() as u32).to_le_bytes());
        footer_payload.extend_from_slice(&schema_json);

        footer_payload.extend_from_slice(&total_rows.to_le_bytes());
        footer_payload.extend_from_slice(&(schema.len() as u32).to_le_bytes());

        for (col_idx, blocks) in column_block_metas.iter().enumerate() {
            footer_payload.extend_from_slice(&(col_idx as u32).to_le_bytes());
            footer_payload.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
            for block in blocks {
                footer_payload.extend_from_slice(&block.offset.to_le_bytes());
                footer_payload.extend_from_slice(&block.frame_len.to_le_bytes());
                footer_payload.extend_from_slice(&block.row_start.to_le_bytes());
                footer_payload.extend_from_slice(&block.row_count.to_le_bytes());
                let enc_tag = match block.encoding {
                    ColumnEncoding::Plain => 0u8,
                    ColumnEncoding::Dictionary => 1u8,
                };
                footer_payload.push(enc_tag);
                footer_payload.extend_from_slice(&block.raw_bytes.to_le_bytes());
                footer_payload.extend_from_slice(&block.stored_bytes.to_le_bytes());
                footer_payload.extend_from_slice(&block.crc32c.to_le_bytes());
                footer_payload.push(if block.has_null { 1 } else { 0 });
                footer_payload.push(if block.has_not_null { 1 } else { 0 });
                if block.has_not_null {
                    encode_typed_value(block.min_value.as_ref().unwrap(), &mut footer_payload)?;
                    encode_typed_value(block.max_value.as_ref().unwrap(), &mut footer_payload)?;
                }
            }
        }

        if footer_payload.len() > MAX_FOOTER_BYTES {
            return Err(HtapError::InvalidArgument(format!(
                "footer payload size {} exceeds MAX_FOOTER_BYTES {MAX_FOOTER_BYTES}",
                footer_payload.len()
            )));
        }

        let footer_len = footer_payload.len() as u32;
        let footer_crc = crc32c::crc32c(&footer_payload);

        writer.write_all(&footer_payload)?;

        // Write trailer
        let mut trailer = [0u8; TRAILER_LEN];
        trailer[0..4].copy_from_slice(&footer_len.to_le_bytes());
        trailer[4..8].copy_from_slice(&footer_crc.to_le_bytes());
        trailer[8..12].copy_from_slice(TRAILER_MAGIC);
        writer.write_all(&trailer)?;

        Ok(SegmentMetadata {
            path: path.to_path_buf(),
            row_count: total_rows,
            schema: schema.clone(),
            column_count: schema.len(),
            block_count: num_row_blocks,
        })
    }

    fn flush_block(
        writer: &mut BufWriter<File>,
        schema: &Schema,
        rows: &[Row],
        row_start: u64,
        options: &SegmentOptions,
        current_offset: &mut u64,
        column_block_metas: &mut [Vec<BlockMeta>],
    ) -> Result<()> {
        let block_row_count = rows.len() as u32;

        for (col_idx, col_def) in schema.columns().iter().enumerate() {
            let mut non_null_values = Vec::new();
            let mut validity = Vec::with_capacity(rows.len());
            let mut has_null = false;
            let mut has_not_null = false;
            let mut min_val: Option<Value> = None;
            let mut max_val: Option<Value> = None;

            for row in rows {
                let val = row
                    .get(col_idx)
                    .expect("col_idx within row length checked by validate_row");
                if val.is_null() {
                    has_null = true;
                    validity.push(false);
                } else {
                    has_not_null = true;
                    validity.push(true);
                    if let Some(ref m) = min_val {
                        if val < m {
                            min_val = Some(val.clone());
                        }
                    } else {
                        min_val = Some(val.clone());
                    }
                    if let Some(ref m) = max_val {
                        if val > m {
                            max_val = Some(val.clone());
                        }
                    } else {
                        max_val = Some(val.clone());
                    }
                    non_null_values.push(val.clone());
                }
            }

            let bitmap = encode_null_bitmap(&validity);
            let null_bitmap_len = bitmap.len() as u32;

            // Choose encoding: dictionary is considered only for String and Bytes
            let (raw_payload, encoding) = match col_def.data_type {
                htap_common::DataType::String | htap_common::DataType::Bytes => {
                    let plain = encode_plain(col_def.data_type, &non_null_values)?;
                    let dict = encode_dictionary(col_def.data_type, &non_null_values)?;
                    if dict.len() < plain.len() {
                        (dict, ColumnEncoding::Dictionary)
                    } else {
                        (plain, ColumnEncoding::Plain)
                    }
                }
                _ => {
                    let plain = encode_plain(col_def.data_type, &non_null_values)?;
                    (plain, ColumnEncoding::Plain)
                }
            };

            if raw_payload.len() > MAX_BLOCK_UNCOMPRESSED_BYTES {
                return Err(HtapError::InvalidArgument(format!(
                    "raw block payload size {} exceeds MAX_BLOCK_UNCOMPRESSED_BYTES",
                    raw_payload.len()
                )));
            }

            let (stored_payload, _) = compress_payload(&raw_payload, options.zstd_level)?;
            if stored_payload.len() > MAX_BLOCK_STORED_BYTES {
                return Err(HtapError::InvalidArgument(format!(
                    "stored block payload size {} exceeds MAX_BLOCK_STORED_BYTES",
                    stored_payload.len()
                )));
            }

            let mut body = Vec::with_capacity(bitmap.len() + stored_payload.len());
            body.extend_from_slice(&bitmap);
            body.extend_from_slice(&stored_payload);

            let crc = crc32c::crc32c(&body);
            let frame_len = (FRAME_HEADER_LEN + body.len()) as u32;

            let mut header_buf = [0u8; FRAME_HEADER_LEN];
            header_buf[0..4].copy_from_slice(&(col_idx as u32).to_le_bytes());
            header_buf[4..12].copy_from_slice(&row_start.to_le_bytes());
            header_buf[12..16].copy_from_slice(&block_row_count.to_le_bytes());
            header_buf[16] = match encoding {
                ColumnEncoding::Plain => 0u8,
                ColumnEncoding::Dictionary => 1u8,
            };
            header_buf[17..21].copy_from_slice(&null_bitmap_len.to_le_bytes());
            header_buf[21..25].copy_from_slice(&(raw_payload.len() as u32).to_le_bytes());
            header_buf[25..29].copy_from_slice(&(stored_payload.len() as u32).to_le_bytes());
            header_buf[29..33].copy_from_slice(&crc.to_le_bytes());

            writer.write_all(&header_buf)?;
            writer.write_all(&body)?;

            column_block_metas[col_idx].push(BlockMeta {
                offset: *current_offset,
                frame_len,
                row_start,
                row_count: block_row_count,
                encoding,
                raw_bytes: raw_payload.len() as u32,
                stored_bytes: stored_payload.len() as u32,
                crc32c: crc,
                has_null,
                has_not_null,
                min_value: min_val,
                max_value: max_val,
            });

            *current_offset += frame_len as u64;
        }

        Ok(())
    }
}

/// Reader for inspecting and decoding durable columnar segment files.
pub struct SegmentReader {
    metadata: SegmentMetadata,
    file: Mutex<File>,
    column_blocks: Vec<Vec<BlockMeta>>,
}

impl SegmentReader {
    /// Opens and validates a columnar segment file.
    ///
    /// Defensively validates the leading header, trailing footer, CRC checksums,
    /// bounds, schema reconstruction, footer exhaustiveness, contiguous row ordinals,
    /// exact row coverage, non-overlapping frame positions, and zone map states.
    ///
    /// # Errors
    /// Returns [`HtapError::Corruption`] if any layout constraint, magic, or CRC fails.
    /// Returns [`HtapError::Io`] on file reading errors.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len();

        let min_len = (HEADER_LEN + TRAILER_LEN) as u64;
        if file_len < min_len {
            return Err(HtapError::Corruption(format!(
                "segment file {} too short ({} bytes, minimum {min_len})",
                path.display(),
                file_len
            )));
        }

        // 1. Validate header magic
        let mut header = [0u8; HEADER_LEN];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header)?;
        if &header != HEADER_MAGIC {
            return Err(HtapError::Corruption(format!(
                "invalid segment header magic in {}",
                path.display()
            )));
        }

        // 2. Validate trailer
        let mut trailer = [0u8; TRAILER_LEN];
        file.seek(SeekFrom::Start(file_len - TRAILER_LEN as u64))?;
        file.read_exact(&mut trailer)?;

        if &trailer[8..12] != TRAILER_MAGIC {
            return Err(HtapError::Corruption(format!(
                "invalid segment trailer magic in {}",
                path.display()
            )));
        }

        let footer_len = u32::from_le_bytes(trailer[0..4].try_into().unwrap()) as usize;
        let footer_crc = u32::from_le_bytes(trailer[4..8].try_into().unwrap());

        if footer_len > MAX_FOOTER_BYTES {
            return Err(HtapError::Corruption(format!(
                "footer length {footer_len} exceeds MAX_FOOTER_BYTES {MAX_FOOTER_BYTES}"
            )));
        }

        let footer_offset = file_len
            .checked_sub((TRAILER_LEN + footer_len) as u64)
            .ok_or_else(|| {
                HtapError::Corruption(format!(
                    "footer length {footer_len} exceeds available file capacity {file_len}"
                ))
            })?;

        if footer_offset < HEADER_LEN as u64 {
            return Err(HtapError::Corruption(format!(
                "footer offset {footer_offset} precedes header"
            )));
        }

        // 3. Read footer payload and verify CRC
        let mut footer_payload = vec![0u8; footer_len];
        file.seek(SeekFrom::Start(footer_offset))?;
        file.read_exact(&mut footer_payload)?;

        let actual_footer_crc = crc32c::crc32c(&footer_payload);
        if actual_footer_crc != footer_crc {
            return Err(HtapError::Corruption(format!(
                "footer CRC mismatch in {}: expected {footer_crc:#010x}, got {actual_footer_crc:#010x}",
                path.display()
            )));
        }

        // 4. Parse footer payload
        let mut reader = htap_common::bytecursor::ByteReader::new(&footer_payload);
        let format_version = reader
            .read_u16_le()
            .map_err(|_| HtapError::Corruption("unexpected EOF reading format version".into()))?;
        if format_version != FORMAT_VERSION {
            return Err(HtapError::Corruption(format!(
                "unsupported format version {format_version} (expected {FORMAT_VERSION})"
            )));
        }

        let schema_len = reader
            .read_u32_le()
            .map_err(|_| HtapError::Corruption("unexpected EOF reading schema length".into()))?
            as usize;
        let schema_payload = reader
            .read_bytes(schema_len)
            .map_err(|_| HtapError::Corruption("unexpected EOF reading schema payload".into()))?;
        let schema: Schema = serde_json::from_slice(schema_payload)
            .map_err(|e| HtapError::Corruption(format!("failed to deserialize schema: {e}")))?;
        validate_segment_schema(&schema)?;

        let total_rows = reader
            .read_u64_le()
            .map_err(|_| HtapError::Corruption("unexpected EOF reading total rows".into()))?;

        let column_count = reader
            .read_u32_le()
            .map_err(|_| HtapError::Corruption("unexpected EOF reading column count".into()))?
            as usize;
        if column_count != schema.len() {
            return Err(HtapError::Corruption(format!(
                "column count {column_count} does not match schema column count {}",
                schema.len()
            )));
        }
        if column_count > MAX_SEGMENT_COLUMNS {
            return Err(HtapError::Corruption(format!(
                "column count {column_count} exceeds MAX_SEGMENT_COLUMNS {MAX_SEGMENT_COLUMNS}"
            )));
        }

        let mut column_blocks: Vec<Vec<BlockMeta>> = Vec::with_capacity(column_count);
        let mut expected_num_blocks: Option<usize> = None;
        let mut total_frames_count: usize = 0;

        for (expected_col_idx, col_def) in schema.columns().iter().enumerate() {
            let col_idx = reader
                .read_u32_le()
                .map_err(|_| HtapError::Corruption("unexpected EOF reading column index".into()))?
                as usize;
            if col_idx != expected_col_idx {
                return Err(HtapError::Corruption(format!(
                    "column index mismatch in footer: expected {expected_col_idx}, got {col_idx}"
                )));
            }

            let block_count = reader.read_u32_le().map_err(|_| {
                HtapError::Corruption("unexpected EOF reading column block count".into())
            })? as usize;

            total_frames_count += block_count;
            if total_frames_count > MAX_SEGMENT_BLOCKS {
                return Err(HtapError::Corruption(format!(
                    "total blocks {total_frames_count} exceeds MAX_SEGMENT_BLOCKS {MAX_SEGMENT_BLOCKS}"
                )));
            }

            if total_rows == 0 {
                if block_count != 0 {
                    return Err(HtapError::Corruption(format!(
                        "zero-row segment has non-zero block count {block_count}"
                    )));
                }
            } else if block_count == 0 {
                return Err(HtapError::Corruption(
                    "non-zero row segment has zero block count".into(),
                ));
            }

            if let Some(exp) = expected_num_blocks {
                if block_count != exp {
                    return Err(HtapError::Corruption(format!(
                        "column {col_idx} has {block_count} blocks, expected {exp}"
                    )));
                }
            } else {
                expected_num_blocks = Some(block_count);
            }

            let mut blocks = Vec::with_capacity(block_count);
            let mut expected_row_start: u64 = 0;

            #[allow(clippy::needless_range_loop)]
            for b_idx in 0..block_count {
                let block_header = reader.read_bytes(39).map_err(|_| {
                    HtapError::Corruption("unexpected EOF reading block entry header".into())
                })?;
                let mut block_reader = htap_common::bytecursor::ByteReader::new(block_header);
                let offset = block_reader
                    .read_u64_le()
                    .expect("fixed-size block entry header");
                let frame_len = block_reader
                    .read_u32_le()
                    .expect("fixed-size block entry header");
                let row_start = block_reader
                    .read_u64_le()
                    .expect("fixed-size block entry header");
                let row_count = block_reader
                    .read_u32_le()
                    .expect("fixed-size block entry header");
                let enc_byte = block_reader
                    .read_u8()
                    .expect("fixed-size block entry header");
                let encoding = match enc_byte {
                    0 => ColumnEncoding::Plain,
                    1 => ColumnEncoding::Dictionary,
                    _ => {
                        return Err(HtapError::Corruption(format!(
                            "invalid encoding tag {enc_byte} in footer block {b_idx}"
                        )))
                    }
                };

                if col_def.data_type != htap_common::DataType::String
                    && col_def.data_type != htap_common::DataType::Bytes
                    && encoding == ColumnEncoding::Dictionary
                {
                    return Err(HtapError::Corruption(format!(
                        "dictionary encoding not allowed for column '{}' with type {}",
                        col_def.name, col_def.data_type
                    )));
                }

                let raw_bytes = block_reader
                    .read_u32_le()
                    .expect("fixed-size block entry header");
                let stored_bytes = block_reader
                    .read_u32_le()
                    .expect("fixed-size block entry header");
                let crc32c = block_reader
                    .read_u32_le()
                    .expect("fixed-size block entry header");
                let has_null_byte = block_reader
                    .read_u8()
                    .expect("fixed-size block entry header");
                let has_not_null_byte = block_reader
                    .read_u8()
                    .expect("fixed-size block entry header");

                let has_null = match has_null_byte {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(HtapError::Corruption(format!(
                            "invalid has_null byte {has_null_byte} in footer block {b_idx}"
                        )))
                    }
                };
                let has_not_null = match has_not_null_byte {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(HtapError::Corruption(format!(
                            "invalid has_not_null byte {has_not_null_byte} in footer block {b_idx}"
                        )))
                    }
                };

                if !has_null && !has_not_null {
                    return Err(HtapError::Corruption(format!(
                        "block {b_idx} of column {col_idx} has neither null nor non-null values"
                    )));
                }
                if !col_def.nullable && has_null {
                    return Err(HtapError::Corruption(format!(
                        "non-nullable column '{}' contains nulls in block {b_idx}",
                        col_def.name
                    )));
                }

                let (min_val, max_val) = if has_not_null {
                    let mut typed_cursor = reader.position();
                    let min_v =
                        decode_typed_value(col_def.data_type, &footer_payload, &mut typed_cursor)?;
                    let max_v =
                        decode_typed_value(col_def.data_type, &footer_payload, &mut typed_cursor)?;
                    reader
                        .read_bytes(typed_cursor - reader.position())
                        .map_err(|_| {
                            HtapError::Corruption(
                                "unexpected EOF reading block zone map values".into(),
                            )
                        })?;
                    if min_v.data_type() != Some(col_def.data_type)
                        || max_v.data_type() != Some(col_def.data_type)
                    {
                        return Err(HtapError::Corruption(format!(
                            "zone map min/max type mismatch for column '{}'",
                            col_def.name
                        )));
                    }
                    if min_v > max_v {
                        return Err(HtapError::Corruption(format!(
                            "zone map min value {min_v:?} exceeds max value {max_v:?} for column '{}'",
                            col_def.name
                        )));
                    }
                    (Some(min_v), Some(max_v))
                } else {
                    (None, None)
                };

                // Validate ordinal coverage
                if row_start != expected_row_start {
                    return Err(HtapError::Corruption(format!(
                        "row ordinal gap in column {col_idx} block {b_idx}: expected {expected_row_start}, got {row_start}"
                    )));
                }
                if row_count == 0 || row_count as usize > MAX_BLOCK_ROWS {
                    return Err(HtapError::Corruption(format!(
                        "invalid row_count {row_count} in column {col_idx} block {b_idx}"
                    )));
                }
                expected_row_start += row_count as u64;

                // Validate alignment with column 0
                if col_idx > 0 {
                    let col0_block = &column_blocks[0][b_idx];
                    if row_start != col0_block.row_start || row_count != col0_block.row_count {
                        return Err(HtapError::Corruption(format!(
                            "column {col_idx} block {b_idx} row range [{row_start}..{}] does not match column 0 [{col0_block:?}]",
                            row_start + row_count as u64
                        )));
                    }
                }

                // Bounds checks on offsets and lengths
                if offset < HEADER_LEN as u64 {
                    return Err(HtapError::Corruption(format!(
                        "block frame offset {offset} precedes header"
                    )));
                }
                if offset.saturating_add(frame_len as u64) > footer_offset {
                    return Err(HtapError::Corruption(format!(
                        "block frame [{offset}..{}] overlaps footer at {footer_offset}",
                        offset + frame_len as u64
                    )));
                }
                if stored_bytes > raw_bytes {
                    return Err(HtapError::Corruption(format!(
                        "stored_bytes {stored_bytes} exceeds raw_bytes {raw_bytes}"
                    )));
                }
                if stored_bytes as usize > MAX_BLOCK_STORED_BYTES {
                    return Err(HtapError::Corruption(format!(
                        "stored_bytes {stored_bytes} exceeds MAX_BLOCK_STORED_BYTES"
                    )));
                }
                if raw_bytes as usize > MAX_BLOCK_UNCOMPRESSED_BYTES {
                    return Err(HtapError::Corruption(format!(
                        "raw_bytes {raw_bytes} exceeds MAX_BLOCK_UNCOMPRESSED_BYTES"
                    )));
                }

                let expected_bitmap_len = (row_count as usize).div_ceil(8);
                let expected_frame_len =
                    FRAME_HEADER_LEN as u32 + expected_bitmap_len as u32 + stored_bytes;
                if frame_len != expected_frame_len {
                    return Err(HtapError::Corruption(format!(
                        "frame length {frame_len} mismatch with computed expected {expected_frame_len}"
                    )));
                }

                blocks.push(BlockMeta {
                    offset,
                    frame_len,
                    row_start,
                    row_count,
                    encoding,
                    raw_bytes,
                    stored_bytes,
                    crc32c,
                    has_null,
                    has_not_null,
                    min_value: min_val,
                    max_value: max_val,
                });
            }

            if expected_row_start != total_rows {
                return Err(HtapError::Corruption(format!(
                    "column {col_idx} row coverage {expected_row_start} does not match total rows {total_rows}"
                )));
            }

            column_blocks.push(blocks);
        }

        if reader.expect_exhausted().is_err() {
            return Err(HtapError::Corruption(format!(
                "trailing bytes in footer payload: consumed {}, total {}",
                reader.position(),
                footer_payload.len()
            )));
        }

        // Validate non-overlapping frames across all columns
        let mut all_frames: Vec<(u64, u32)> = Vec::with_capacity(total_frames_count);
        for col_blocks in &column_blocks {
            for b in col_blocks {
                all_frames.push((b.offset, b.frame_len));
            }
        }
        all_frames.sort_by_key(|&(off, _)| off);
        for i in 0..all_frames.len().saturating_sub(1) {
            let (curr_off, curr_len) = all_frames[i];
            let (next_off, _) = all_frames[i + 1];
            if curr_off.saturating_add(curr_len as u64) > next_off {
                return Err(HtapError::Corruption(format!(
                    "overlapping block frames detected: frame at {curr_off} (len {curr_len}) overlaps frame at {next_off}"
                )));
            }
        }

        let num_row_blocks = expected_num_blocks.unwrap_or(0);
        let metadata = SegmentMetadata {
            path,
            row_count: total_rows,
            schema,
            column_count,
            block_count: num_row_blocks,
        };

        Ok(Self {
            metadata,
            file: Mutex::new(file),
            column_blocks,
        })
    }

    /// Returns a reference to the segment metadata.
    #[must_use]
    pub fn metadata(&self) -> &SegmentMetadata {
        &self.metadata
    }

    /// Returns a reference to the segment schema.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.metadata.schema
    }

    /// Returns the number of row-aligned blocks in the segment.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.metadata.block_count
    }

    /// Returns a reference to the metadata for a specific block of a column, or `None` if out of bounds.
    #[must_use]
    pub fn block_meta(&self, column_idx: usize, block_idx: usize) -> Option<&BlockMeta> {
        self.column_blocks
            .get(column_idx)
            .and_then(|blocks| blocks.get(block_idx))
    }

    fn read_and_verify_frame(&self, column_idx: usize, block_idx: usize) -> Result<VerifiedFrame> {
        let meta = self
            .block_meta(column_idx, block_idx)
            .ok_or_else(|| {
                HtapError::InvalidArgument(format!(
                    "block index ({column_idx}, {block_idx}) out of range (columns: {}, blocks: {})",
                    self.metadata.column_count, self.metadata.block_count
                ))
            })?
            .clone();

        let col_def = self
            .metadata
            .schema
            .column(column_idx)
            .expect("column index verified within schema bounds")
            .clone();

        let mut file = self
            .file
            .lock()
            .map_err(|e| HtapError::Internal(e.to_string()))?;
        file.seek(SeekFrom::Start(meta.offset))?;

        let mut frame_header = [0u8; FRAME_HEADER_LEN];
        file.read_exact(&mut frame_header)?;

        let hdr_col_idx = u32::from_le_bytes(frame_header[0..4].try_into().unwrap()) as usize;
        let hdr_row_start = u64::from_le_bytes(frame_header[4..12].try_into().unwrap());
        let hdr_row_count = u32::from_le_bytes(frame_header[12..16].try_into().unwrap());
        let hdr_enc_byte = frame_header[16];
        let hdr_null_bm_len = u32::from_le_bytes(frame_header[17..21].try_into().unwrap()) as usize;
        let hdr_raw_payload_len =
            u32::from_le_bytes(frame_header[21..25].try_into().unwrap()) as usize;
        let hdr_stored_payload_len =
            u32::from_le_bytes(frame_header[25..29].try_into().unwrap()) as usize;
        let hdr_crc = u32::from_le_bytes(frame_header[29..33].try_into().unwrap());

        let hdr_encoding = match hdr_enc_byte {
            0 => ColumnEncoding::Plain,
            1 => ColumnEncoding::Dictionary,
            _ => {
                return Err(HtapError::Corruption(format!(
                    "invalid frame encoding tag {hdr_enc_byte}"
                )))
            }
        };

        // Validate frame/footer agreement
        if hdr_col_idx != column_idx
            || hdr_row_start != meta.row_start
            || hdr_row_count != meta.row_count
            || hdr_encoding != meta.encoding
            || hdr_raw_payload_len != meta.raw_bytes as usize
            || hdr_stored_payload_len != meta.stored_bytes as usize
            || hdr_crc != meta.crc32c
        {
            return Err(HtapError::Corruption(
                "block frame header does not agree with footer metadata".into(),
            ));
        }

        let expected_bm_len = (meta.row_count as usize).div_ceil(8);
        if hdr_null_bm_len != expected_bm_len {
            return Err(HtapError::Corruption(format!(
                "null bitmap length {hdr_null_bm_len} does not match expected {expected_bm_len}"
            )));
        }

        if hdr_stored_payload_len > MAX_BLOCK_STORED_BYTES
            || hdr_raw_payload_len > MAX_BLOCK_UNCOMPRESSED_BYTES
            || hdr_stored_payload_len > hdr_raw_payload_len
        {
            return Err(HtapError::Corruption(
                "frame payload lengths exceed configured limits".into(),
            ));
        }

        let body_len = hdr_null_bm_len + hdr_stored_payload_len;
        let mut body = vec![0u8; body_len];
        file.read_exact(&mut body)?;

        // Verify CRC before decompression
        let actual_crc = crc32c::crc32c(&body);
        if actual_crc != meta.crc32c {
            return Err(HtapError::Corruption(format!(
                "block frame CRC mismatch: expected {:#010x}, got {actual_crc:#010x}",
                meta.crc32c
            )));
        }

        // Decode null bitmap
        let validity = decode_null_bitmap(&body[..hdr_null_bm_len], meta.row_count as usize)?;
        let non_null_count = validity.iter().filter(|&&v| v).count();

        // Verify zone map states agreement
        let actual_has_null = non_null_count < meta.row_count as usize;
        let actual_has_not_null = non_null_count > 0;
        if actual_has_null != meta.has_null || actual_has_not_null != meta.has_not_null {
            return Err(HtapError::Corruption(
                "block null bitmap does not agree with zone map nullability states".into(),
            ));
        }
        if !col_def.nullable && actual_has_null {
            return Err(HtapError::Corruption(format!(
                "non-nullable column '{}' contains nulls in block body",
                col_def.name
            )));
        }

        let stored_payload = body[hdr_null_bm_len..].to_vec();

        Ok(VerifiedFrame {
            meta,
            col_def,
            validity,
            non_null_count,
            stored_payload,
        })
    }

    pub(crate) fn read_block_validity(
        &self,
        column_idx: usize,
        block_idx: usize,
    ) -> Result<Vec<bool>> {
        let frame = self.read_and_verify_frame(column_idx, block_idx)?;
        Ok(frame.validity)
    }

    pub(crate) fn read_block_internal(
        &self,
        column_idx: usize,
        block_idx: usize,
    ) -> Result<ColumnVector> {
        let frame = self.read_and_verify_frame(column_idx, block_idx)?;

        // Decompress payload
        let raw_payload = decompress_payload(&frame.stored_payload, frame.meta.raw_bytes as usize)?;

        // Decode values
        let non_null_values = match frame.meta.encoding {
            ColumnEncoding::Plain => {
                decode_plain(frame.col_def.data_type, &raw_payload, frame.non_null_count)?
            }
            ColumnEncoding::Dictionary => {
                decode_dictionary(frame.col_def.data_type, &raw_payload, frame.non_null_count)?
            }
        };

        if non_null_values.len() != frame.non_null_count {
            return Err(HtapError::Corruption(format!(
                "decoded non-null values count {} does not match bitmap non-null count {}",
                non_null_values.len(),
                frame.non_null_count
            )));
        }

        // Validate non-null values against zone map bounds
        if frame.meta.has_not_null {
            let min_v = frame.meta.min_value.as_ref().unwrap();
            let max_v = frame.meta.max_value.as_ref().unwrap();
            for v in &non_null_values {
                if v < min_v || v > max_v {
                    return Err(HtapError::Corruption(format!(
                        "decoded value {v:?} falls outside zone map bounds [{min_v:?}, {max_v:?}]"
                    )));
                }
            }
        }

        build_column_vector(frame.col_def.data_type, frame.validity, non_null_values)
    }

    /// Reads, verifies, and decodes a specific column block into a [`ColumnVector`].
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if indices are out of bounds.
    /// Returns [`HtapError::Corruption`] if CRC, size limits, bitmap agreement, or encodings fail validation.
    /// Returns [`HtapError::Io`] on file read errors.
    pub fn read_block(&mut self, column_idx: usize, block_idx: usize) -> Result<ColumnVector> {
        self.read_block_internal(column_idx, block_idx)
    }

    /// Executes a vectorized scan with conservative zone-map pushdown and selective decoding.
    ///
    /// # Errors
    /// Returns [`HtapError::InvalidArgument`] if `request` fails validation against the segment schema.
    /// Returns [`HtapError::Corruption`] if any decoded block frame fails integrity or decompression checks.
    /// Returns [`HtapError::Io`] on file read errors.
    pub fn scan(&self, request: &ScanRequest) -> Result<ScanResult> {
        crate::scan::execute_scan(self, request)
    }
}

struct VerifiedFrame {
    meta: BlockMeta,
    col_def: ColumnDef,
    validity: Vec<bool>,
    non_null_count: usize,
    stored_payload: Vec<u8>,
}

fn build_column_vector(
    data_type: DataType,
    validity: Vec<bool>,
    non_null_values: Vec<Value>,
) -> Result<ColumnVector> {
    let mut val_iter = non_null_values.into_iter();
    let vector = match data_type {
        DataType::Bool => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::Bool(b)) = val_iter.next() {
                        values.push(b);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing bool vector".into(),
                        ));
                    }
                } else {
                    values.push(false);
                }
            }
            ColumnVector::Bool { values, validity }
        }
        DataType::Int32 => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::Int32(i)) = val_iter.next() {
                        values.push(i);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing int32 vector".into(),
                        ));
                    }
                } else {
                    values.push(0);
                }
            }
            ColumnVector::Int32 { values, validity }
        }
        DataType::Int64 => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::Int64(i)) = val_iter.next() {
                        values.push(i);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing int64 vector".into(),
                        ));
                    }
                } else {
                    values.push(0);
                }
            }
            ColumnVector::Int64 { values, validity }
        }
        DataType::Timestamp => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::Timestamp(t)) = val_iter.next() {
                        values.push(t);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing timestamp vector".into(),
                        ));
                    }
                } else {
                    values.push(0);
                }
            }
            ColumnVector::Timestamp { values, validity }
        }
        DataType::Float64 => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::Float64(f)) = val_iter.next() {
                        values.push(f);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing float64 vector".into(),
                        ));
                    }
                } else {
                    values.push(0.0);
                }
            }
            ColumnVector::Float64 { values, validity }
        }
        DataType::String => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::String(s)) = val_iter.next() {
                        values.push(s);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing string vector".into(),
                        ));
                    }
                } else {
                    values.push(String::new());
                }
            }
            ColumnVector::String { values, validity }
        }
        DataType::Bytes => {
            let mut values = Vec::with_capacity(validity.len());
            for &is_valid in &validity {
                if is_valid {
                    if let Some(Value::Bytes(b)) = val_iter.next() {
                        values.push(b);
                    } else {
                        return Err(HtapError::Corruption(
                            "type mismatch reconstructing bytes vector".into(),
                        ));
                    }
                } else {
                    values.push(Vec::new());
                }
            }
            ColumnVector::Bytes { values, validity }
        }
    };

    vector.validate()?;
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::{ColumnDef, DataType};
    use tempfile::tempdir;

    fn test_col(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type,
            nullable,
            primary_key: false,
        }
    }

    #[test]
    fn test_zero_row_segment() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("zero_rows.col");
        let schema = Schema::new(vec![
            test_col("c1", DataType::Int32, false),
            test_col("c2", DataType::String, true),
        ])
        .unwrap();

        let opts = SegmentOptions::new();
        let meta = SegmentWriter::write(&path, &schema, Vec::<Row>::new(), &opts).unwrap();
        assert_eq!(meta.row_count, 0);
        assert_eq!(meta.block_count, 0);
        assert_eq!(meta.column_count, 2);

        let mut reader = SegmentReader::open(&path).unwrap();
        assert_eq!(reader.metadata().row_count, 0);
        assert_eq!(reader.metadata().block_count, 0);
        assert_eq!(reader.schema(), &schema);
        assert!(reader.read_block(0, 0).is_err());
    }

    #[test]
    fn test_segment_writer_cleans_up_on_failure() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("failed.col");
        let schema = Schema::new(vec![test_col("c1", DataType::Int32, false)]).unwrap();

        // Row has invalid data type (String instead of Int32)
        let rows = vec![Row::new(vec![Value::String("bad".into())])];
        let opts = SegmentOptions::new();
        let res = SegmentWriter::write(&path, &schema, rows, &opts);
        assert!(res.is_err());
        assert!(!path.exists(), "partial file must be removed on failure");
    }
}
