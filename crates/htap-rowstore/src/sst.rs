//! Sorted string table (SST) writer and reader.
//!
//! SSTs are immutable, sorted on-disk files storing rows and tombstones for the LSM row store.
//!
//! # On-Disk Layout (SST v1)
//!
//! All integers are little-endian. Keys are preserved bytewise without decoding.
//!
//! ```text
//! file := header data_block_frame* footer_payload footer_trailer
//!
//! header := magic[8] = "HTAPSST1"
//!
//! data_block_frame :=
//!   payload_len: u32
//!   crc32c: u32                  // CRC32-C over payload bytes only
//!   payload: [u8; payload_len]
//!
//! data block payload :=
//!   entry_count: u32
//!   entry*
//!
//! entry :=
//!   partition_id: u64
//!   user_key_len: u32
//!   user_key: [u8; user_key_len]
//!   version: u64
//!   kind: u8                     // 0 = Put, 1 = Delete
//!   row_json_len: u32            // 0 for Delete
//!   row_json: [u8; row_json_len] // serde_json of Row, Put only
//!
//! footer payload :=
//!   format_version: u16 = 1
//!   sst_id: u64
//!   entry_count: u64
//!   block_count: u32
//!   bloom_hash_count: u8
//!   bloom_bit_count: u64
//!   bloom_byte_len: u32
//!   bloom_bytes: [u8; bloom_byte_len]
//!   block_index_entry*
//!
//! block index entry :=
//!   block_offset: u64            // offset of the data_block_frame
//!   block_frame_len: u32         // 8-byte frame header + payload
//!   entry_count: u32
//!   first_partition_id: u64
//!   first_user_key_len: u32
//!   first_user_key: [...]
//!   last_partition_id: u64
//!   last_user_key_len: u32
//!   last_user_key: [...]
//!   min_version: u64
//!   max_version: u64
//!
//! footer trailer :=
//!   footer_len: u32
//!   footer_crc32c: u32           // CRC32-C over footer payload only
//!   magic[4] = "HEND"
//! ```
//!
//! # Corruption and Durability (Contrast with WAL)
//!
//! In the write-ahead log ([`crate::wal`]), a torn tail or partial record at the end of an
//! active segment is an expected consequence of crashing during `write(2)` and is cleanly
//! truncated during replay.
//!
//! In contrast, an SST file is published only after a full [`std::fs::File::sync_all`]. Any
//! truncation, bad magic, invalid tag, out-of-order entry, or CRC mismatch in an SST file is
//! **unconditional corruption** and returns [`HtapError::Corruption`]. There is no silent
//! truncation of SST files.
//!
//! # Publication
//!
//! [`SstWriter::write`] writes directly to the destination path and fsyncs the file.
//! Atomic publication via temporary staging files, directory fsyncs, and manifest registration
//! is managed by higher-level LSM engine components in a later task.
//!
//! # Tombstones
//!
//! Deletion tombstones ([`ValueKind::Delete`]) are load-bearing: visible tombstones are
//! returned as `Some(MemtableEntry { value: ValueKind::Delete, .. })`, never `None`.
//! Collapsing tombstones to `None` would cause read queries to fall through to older SST
//! levels and resurrect previously deleted records.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use htap_common::{HtapError, Result, Row, Version};

use crate::memtable::{InternalKey, MemtableEntry, ValueKind};

/// Default target data block size in bytes (64 KiB).
pub const DEFAULT_SST_BLOCK_BYTES: usize = 64 * 1024;

/// Header magic identifier for SST v1 files.
const HEADER_MAGIC: &[u8; 8] = b"HTAPSST1";

/// Footer trailer magic identifier for SST files.
const TRAILER_MAGIC: &[u8; 4] = b"HEND";

/// Current on-disk SST format version.
const FORMAT_VERSION: u16 = 1;

/// Length of fixed file header in bytes.
const HEADER_LEN: usize = 8;

/// Length of fixed footer trailer in bytes (`footer_len: u32` + `footer_crc32c: u32` + `magic[4]`).
const TRAILER_LEN: usize = 12;

/// Length of fixed data block frame header in bytes (`payload_len: u32` + `crc32c: u32`).
const BLOCK_FRAME_HEADER_LEN: usize = 8;

/// Upper bound on a single data block's payload (64 MiB) to guard against unbounded allocations.
pub const MAX_BLOCK_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Upper bound on the footer payload (64 MiB) to guard against unbounded allocations.
pub const MAX_FOOTER_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;

/// Upper bound on a single user key's length (64 KiB).
pub const MAX_USER_KEY_BYTES: u32 = 64 * 1024;

/// Upper bound on serialized row JSON length (16 MiB).
pub const MAX_ROW_JSON_BYTES: u32 = 16 * 1024 * 1024;

/// Upper bound on the number of blocks allowed in an SST (1 million).
pub const MAX_BLOCK_COUNT: u32 = 1_000_000;

/// Options controlling SST generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstOptions {
    /// Target size in bytes for each data block payload.
    pub block_bytes: usize,
    /// Number of bloom filter bits allocated per entry.
    pub bloom_bits_per_key: usize,
}

impl SstOptions {
    /// Create options with default values (64 KiB block bytes, 10 bloom bits per key).
    pub fn new() -> Self {
        Self {
            block_bytes: DEFAULT_SST_BLOCK_BYTES,
            bloom_bits_per_key: 10,
        }
    }

    /// Set the target block payload size in bytes.
    #[must_use]
    pub fn with_block_bytes(mut self, bytes: usize) -> Self {
        self.block_bytes = bytes;
        self
    }

    /// Set the number of bloom filter bits allocated per key.
    #[must_use]
    pub fn with_bloom_bits_per_key(mut self, bits: usize) -> Self {
        self.bloom_bits_per_key = bits;
        self
    }
}

impl Default for SstOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Metadata summarizing an SST file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstMetadata {
    /// Unique identifier for this SST file.
    pub id: u64,
    /// Path to the SST file.
    pub path: PathBuf,
    /// Total number of entries stored in the SST.
    pub entry_count: u64,
    /// Smallest internal key in the SST, or `None` if empty.
    pub min_key: Option<InternalKey>,
    /// Largest internal key in the SST, or `None` if empty.
    pub max_key: Option<InternalKey>,
    /// Smallest commit version in the SST, or `None` if empty.
    pub min_version: Option<Version>,
    /// Largest commit version in the SST, or `None` if empty.
    pub max_version: Option<Version>,
}

/// Entry stored in the SST block index.
#[derive(Debug, Clone)]
struct BlockIndexEntry {
    block_offset: u64,
    block_frame_len: u32,
    #[allow(dead_code)]
    entry_count: u32,
    first_partition_id: u64,
    first_user_key: Vec<u8>,
    last_partition_id: u64,
    last_user_key: Vec<u8>,
    min_version: u64,
    max_version: u64,
}

/// SST file writer.
pub struct SstWriter {
    _private: (),
}

impl SstWriter {
    /// Write a sorted stream of entries into an SST file at `path`.
    ///
    /// Entries must arrive in strictly ascending order according to [`InternalKey`]'s [`Ord`].
    /// Duplicate or out-of-order keys return [`HtapError::InvalidArgument`].
    ///
    /// Calls [`File::sync_all`] on the file before returning.
    pub fn write(
        path: &Path,
        id: u64,
        entries: impl IntoIterator<Item = MemtableEntry>,
        options: &SstOptions,
    ) -> Result<SstMetadata> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);

        match Self::write_internal(&mut writer, path, id, entries, options) {
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
        id: u64,
        entries: impl IntoIterator<Item = MemtableEntry>,
        options: &SstOptions,
    ) -> Result<SstMetadata> {
        writer.write_all(HEADER_MAGIC)?;

        let mut current_offset = HEADER_LEN as u64;
        let mut current_block_entries: Vec<(MemtableEntry, Vec<u8>)> = Vec::new();
        let mut current_block_payload_len: usize = 4; // 4 bytes for entry_count
        let mut block_indexes: Vec<BlockIndexEntry> = Vec::new();

        let mut entry_count: u64 = 0;
        let mut min_key: Option<InternalKey> = None;
        let mut max_key: Option<InternalKey> = None;
        let mut min_version: Option<Version> = None;
        let mut max_version: Option<Version> = None;
        let mut prev_key: Option<InternalKey> = None;
        let mut hashes: Vec<u64> = Vec::new();

        for entry in entries {
            if let Some(prev) = &prev_key {
                match prev.cmp(&entry.key) {
                    Ordering::Equal => {
                        return Err(HtapError::InvalidArgument(format!(
                            "duplicate internal key: partition {}, user_key {:?}, version {}",
                            entry.key.partition_id, entry.key.user_key, entry.key.version
                        )));
                    }
                    Ordering::Greater => {
                        return Err(HtapError::InvalidArgument(format!(
                            "out-of-order internal key: prev was {prev:?}, current is {:?}",
                            entry.key
                        )));
                    }
                    Ordering::Less => {}
                }
            }

            let encoded_entry = encode_entry(&entry)?;
            let entry_len = encoded_entry.len();

            if !current_block_entries.is_empty()
                && (current_block_payload_len + entry_len > options.block_bytes)
            {
                let (idx_entry, frame_len) =
                    flush_block(writer, &current_block_entries, current_offset)?;
                current_offset += frame_len as u64;
                block_indexes.push(idx_entry);
                current_block_entries.clear();
                current_block_payload_len = 4;
            }

            if min_key.is_none() {
                min_key = Some(entry.key.clone());
            }
            max_key = Some(entry.key.clone());
            min_version = Some(min_version.map_or(entry.key.version, |v| v.min(entry.key.version)));
            max_version = Some(max_version.map_or(entry.key.version, |v| v.max(entry.key.version)));

            hashes.push(fnv1a_64(entry.key.partition_id, &entry.key.user_key));
            prev_key = Some(entry.key.clone());
            current_block_payload_len += entry_len;
            current_block_entries.push((entry, encoded_entry));
            entry_count += 1;
        }

        if !current_block_entries.is_empty() {
            let (idx_entry, _frame_len) =
                flush_block(writer, &current_block_entries, current_offset)?;
            block_indexes.push(idx_entry);
        }

        // Build bloom filter
        let raw_bits = entry_count.saturating_mul(options.bloom_bits_per_key as u64);
        let bloom_byte_len = raw_bits.div_ceil(8).max(1) as u32;
        let bloom_bit_count = (bloom_byte_len as u64) * 8;
        let mut bloom_bytes = vec![0u8; bloom_byte_len as usize];

        let bloom_hash_count = if options.bloom_bits_per_key == 0 {
            1u8
        } else {
            ((options.bloom_bits_per_key as f64 * std::f64::consts::LN_2).round() as u8)
                .clamp(1, 30)
        };

        if entry_count > 0 {
            for h in hashes {
                bloom_set(&mut bloom_bytes, bloom_bit_count, bloom_hash_count, h);
            }
        }

        // Build footer payload
        let mut footer_payload = Vec::new();
        footer_payload.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        footer_payload.extend_from_slice(&id.to_le_bytes());
        footer_payload.extend_from_slice(&entry_count.to_le_bytes());
        footer_payload.extend_from_slice(&(block_indexes.len() as u32).to_le_bytes());
        footer_payload.push(bloom_hash_count);
        footer_payload.extend_from_slice(&bloom_bit_count.to_le_bytes());
        footer_payload.extend_from_slice(&(bloom_bytes.len() as u32).to_le_bytes());
        footer_payload.extend_from_slice(&bloom_bytes);

        for block in &block_indexes {
            footer_payload.extend_from_slice(&block.block_offset.to_le_bytes());
            footer_payload.extend_from_slice(&block.block_frame_len.to_le_bytes());
            footer_payload.extend_from_slice(&block.entry_count.to_le_bytes());
            footer_payload.extend_from_slice(&block.first_partition_id.to_le_bytes());
            footer_payload.extend_from_slice(&(block.first_user_key.len() as u32).to_le_bytes());
            footer_payload.extend_from_slice(&block.first_user_key);
            footer_payload.extend_from_slice(&block.last_partition_id.to_le_bytes());
            footer_payload.extend_from_slice(&(block.last_user_key.len() as u32).to_le_bytes());
            footer_payload.extend_from_slice(&block.last_user_key);
            footer_payload.extend_from_slice(&block.min_version.to_le_bytes());
            footer_payload.extend_from_slice(&block.max_version.to_le_bytes());
        }

        if footer_payload.len() as u64 > MAX_FOOTER_PAYLOAD_BYTES as u64 {
            return Err(HtapError::InvalidArgument(format!(
                "footer payload length {} exceeds maximum {}",
                footer_payload.len(),
                MAX_FOOTER_PAYLOAD_BYTES
            )));
        }

        let footer_len = footer_payload.len() as u32;
        let footer_crc = crc32c::crc32c(&footer_payload);

        writer.write_all(&footer_payload)?;

        let mut trailer = [0u8; TRAILER_LEN];
        trailer[0..4].copy_from_slice(&footer_len.to_le_bytes());
        trailer[4..8].copy_from_slice(&footer_crc.to_le_bytes());
        trailer[8..12].copy_from_slice(TRAILER_MAGIC);
        writer.write_all(&trailer)?;

        Ok(SstMetadata {
            id,
            path: path.to_path_buf(),
            entry_count,
            min_key,
            max_key,
            min_version,
            max_version,
        })
    }
}

/// SST file reader.
#[derive(Debug)]
pub struct SstReader {
    metadata: SstMetadata,
    file: File,
    bloom_hash_count: u8,
    bloom_bit_count: u64,
    bloom_bytes: Vec<u8>,
    block_indexes: Vec<BlockIndexEntry>,
}

impl SstReader {
    /// Open and validate an SST file.
    ///
    /// Validates the trailing magic `"HEND"`, footer length and CRC, format version,
    /// leading magic `"HTAPSST1"`, and bounds of all block index entries before reading.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut file = File::open(&path)?;
        let file_len = file.metadata()?.len();

        if file_len < (HEADER_LEN + TRAILER_LEN) as u64 {
            return Err(HtapError::Corruption(format!(
                "SST file {} is too short ({} bytes)",
                path.display(),
                file_len
            )));
        }

        // 1. Validate trailing trailer
        let mut trailer = [0u8; TRAILER_LEN];
        file.seek(SeekFrom::Start(file_len - TRAILER_LEN as u64))?;
        file.read_exact(&mut trailer)?;

        if &trailer[8..12] != TRAILER_MAGIC {
            return Err(HtapError::Corruption(format!(
                "invalid SST trailing magic in {}",
                path.display()
            )));
        }

        let footer_len = u32::from_le_bytes(trailer[0..4].try_into().unwrap());
        let expected_footer_crc = u32::from_le_bytes(trailer[4..8].try_into().unwrap());

        if footer_len > MAX_FOOTER_PAYLOAD_BYTES {
            return Err(HtapError::Corruption(format!(
                "footer length {footer_len} exceeds limit {MAX_FOOTER_PAYLOAD_BYTES}"
            )));
        }

        let min_required_len = (HEADER_LEN + TRAILER_LEN) as u64 + footer_len as u64;
        if file_len < min_required_len {
            return Err(HtapError::Corruption(format!(
                "file length {file_len} too small for footer length {footer_len}"
            )));
        }

        let footer_offset = file_len - TRAILER_LEN as u64 - footer_len as u64;
        file.seek(SeekFrom::Start(footer_offset))?;
        let mut footer_payload = vec![0u8; footer_len as usize];
        file.read_exact(&mut footer_payload)?;

        if crc32c::crc32c(&footer_payload) != expected_footer_crc {
            return Err(HtapError::Corruption(format!(
                "footer CRC mismatch in {}",
                path.display()
            )));
        }

        // 2. Decode footer payload
        let mut cursor = &footer_payload[..];
        if cursor.len() < 2 + 8 + 8 + 4 + 1 + 8 + 4 {
            return Err(HtapError::Corruption(
                "footer payload too short for fixed fields".into(),
            ));
        }

        let format_version = u16::from_le_bytes(cursor[..2].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(HtapError::Corruption(format!(
                "unsupported SST format version {format_version} (expected {FORMAT_VERSION})"
            )));
        }
        let sst_id = u64::from_le_bytes(cursor[2..10].try_into().unwrap());
        let entry_count = u64::from_le_bytes(cursor[10..18].try_into().unwrap());
        let block_count = u32::from_le_bytes(cursor[18..22].try_into().unwrap());
        let bloom_hash_count = cursor[22];
        let bloom_bit_count = u64::from_le_bytes(cursor[23..31].try_into().unwrap());
        let bloom_byte_len = u32::from_le_bytes(cursor[31..35].try_into().unwrap()) as usize;
        cursor = &cursor[35..];

        if block_count > MAX_BLOCK_COUNT {
            return Err(HtapError::Corruption(format!(
                "block count {block_count} exceeds maximum {MAX_BLOCK_COUNT}"
            )));
        }
        if bloom_byte_len as u64 > MAX_FOOTER_PAYLOAD_BYTES as u64 || cursor.len() < bloom_byte_len
        {
            return Err(HtapError::Corruption(
                "bloom byte length exceeds bounds".into(),
            ));
        }
        let bloom_bytes = cursor[..bloom_byte_len].to_vec();
        cursor = &cursor[bloom_byte_len..];

        let mut block_indexes = Vec::with_capacity(block_count as usize);
        for _ in 0..block_count {
            if cursor.len() < 8 + 4 + 4 + 8 + 4 {
                return Err(HtapError::Corruption(
                    "unexpected end of block index entry".into(),
                ));
            }
            let block_offset = u64::from_le_bytes(cursor[..8].try_into().unwrap());
            let block_frame_len = u32::from_le_bytes(cursor[8..12].try_into().unwrap());
            let block_entry_count = u32::from_le_bytes(cursor[12..16].try_into().unwrap());
            let first_partition_id = u64::from_le_bytes(cursor[16..24].try_into().unwrap());
            let first_user_key_len =
                u32::from_le_bytes(cursor[24..28].try_into().unwrap()) as usize;
            cursor = &cursor[28..];

            if first_user_key_len as u32 > MAX_USER_KEY_BYTES
                || cursor.len() < first_user_key_len + 8 + 4
            {
                return Err(HtapError::Corruption(
                    "invalid first_user_key in block index".into(),
                ));
            }
            let first_user_key = cursor[..first_user_key_len].to_vec();
            cursor = &cursor[first_user_key_len..];

            let last_partition_id = u64::from_le_bytes(cursor[..8].try_into().unwrap());
            let last_user_key_len = u32::from_le_bytes(cursor[8..12].try_into().unwrap()) as usize;
            cursor = &cursor[12..];

            if last_user_key_len as u32 > MAX_USER_KEY_BYTES
                || cursor.len() < last_user_key_len + 8 + 8
            {
                return Err(HtapError::Corruption(
                    "invalid last_user_key in block index".into(),
                ));
            }
            let last_user_key = cursor[..last_user_key_len].to_vec();
            cursor = &cursor[last_user_key_len..];

            let min_version = u64::from_le_bytes(cursor[..8].try_into().unwrap());
            let max_version = u64::from_le_bytes(cursor[8..16].try_into().unwrap());
            cursor = &cursor[16..];

            block_indexes.push(BlockIndexEntry {
                block_offset,
                block_frame_len,
                entry_count: block_entry_count,
                first_partition_id,
                first_user_key,
                last_partition_id,
                last_user_key,
                min_version,
                max_version,
            });
        }

        if !cursor.is_empty() {
            return Err(HtapError::Corruption(format!(
                "trailing unparsed {} bytes in footer payload",
                cursor.len()
            )));
        }

        // 3. Validate leading magic
        let mut header = [0u8; HEADER_LEN];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header)?;
        if &header != HEADER_MAGIC {
            return Err(HtapError::Corruption(format!(
                "invalid SST header magic in {}",
                path.display()
            )));
        }

        // 4. Validate block offsets and lengths
        let mut prev_end = HEADER_LEN as u64;
        let mut prev_last: Option<(u64, &[u8])> = None;

        for (idx, block) in block_indexes.iter().enumerate() {
            if block.block_offset < prev_end {
                return Err(HtapError::Corruption(format!(
                    "block {idx} offset {} overlaps previous end {prev_end}",
                    block.block_offset
                )));
            }
            if (block.block_frame_len as usize) < BLOCK_FRAME_HEADER_LEN {
                return Err(HtapError::Corruption(format!(
                    "block {idx} frame length {} too short",
                    block.block_frame_len
                )));
            }
            let block_end = match block.block_offset.checked_add(block.block_frame_len as u64) {
                Some(end) => end,
                None => return Err(HtapError::Corruption("block offset overflow".into())),
            };
            if block_end > footer_offset {
                return Err(HtapError::Corruption(format!(
                    "block {idx} end {block_end} extends past footer start {footer_offset}"
                )));
            }
            prev_end = block_end;

            if (block.first_partition_id, block.first_user_key.as_slice())
                > (block.last_partition_id, block.last_user_key.as_slice())
            {
                return Err(HtapError::Corruption(format!(
                    "block {idx} first key is greater than last key"
                )));
            }

            if let Some((prev_p, prev_k)) = prev_last {
                if (prev_p, prev_k) > (block.first_partition_id, block.first_user_key.as_slice()) {
                    return Err(HtapError::Corruption(format!(
                        "block index entries out of order between block {} and {idx}",
                        idx - 1
                    )));
                }
            }
            prev_last = Some((block.last_partition_id, &block.last_user_key));
        }

        // 5. Construct metadata
        let (min_key, max_key, min_version, max_version) = if block_count == 0 {
            if entry_count != 0 {
                return Err(HtapError::Corruption(
                    "entry_count > 0 but block_count == 0".into(),
                ));
            }
            (None, None, None, None)
        } else {
            if entry_count == 0 {
                return Err(HtapError::Corruption(
                    "entry_count == 0 but block_count > 0".into(),
                ));
            }
            let first_entries = read_block_at(&mut file, &block_indexes[0])?;
            if first_entries.is_empty() {
                return Err(HtapError::Corruption("first block is empty".into()));
            }
            let min_k = first_entries[0].key.clone();

            let max_k = if block_count == 1 {
                first_entries.last().unwrap().key.clone()
            } else {
                let last_entries =
                    read_block_at(&mut file, &block_indexes[block_count as usize - 1])?;
                if last_entries.is_empty() {
                    return Err(HtapError::Corruption("last block is empty".into()));
                }
                last_entries.last().unwrap().key.clone()
            };

            let min_v = Some(Version::new(
                block_indexes.iter().map(|b| b.min_version).min().unwrap(),
            ));
            let max_v = Some(Version::new(
                block_indexes.iter().map(|b| b.max_version).max().unwrap(),
            ));
            (Some(min_k), Some(max_k), min_v, max_v)
        };

        let metadata = SstMetadata {
            id: sst_id,
            path,
            entry_count,
            min_key,
            max_key,
            min_version,
            max_version,
        };

        Ok(SstReader {
            metadata,
            file,
            bloom_hash_count,
            bloom_bit_count,
            bloom_bytes,
            block_indexes,
        })
    }

    /// Return the metadata of this SST.
    pub fn metadata(&self) -> &SstMetadata {
        &self.metadata
    }

    /// Retrieve the visible entry for `(partition_id, user_key)` at `snapshot`.
    ///
    /// Checks the bloom filter first; on positive result, binary-searches the block
    /// index for candidate blocks and returns the entry with the highest `version <= snapshot`.
    ///
    /// Tombstones ([`ValueKind::Delete`]) are returned as `Some(entry)`, never `None`.
    pub fn get(
        &self,
        partition_id: u64,
        user_key: &[u8],
        snapshot: Version,
    ) -> Result<Option<MemtableEntry>> {
        if self.metadata.entry_count == 0 || self.block_indexes.is_empty() {
            return Ok(None);
        }

        // Bloom filter check
        let hash = fnv1a_64(partition_id, user_key);
        if !bloom_check(
            &self.bloom_bytes,
            self.bloom_bit_count,
            self.bloom_hash_count,
            hash,
        ) {
            return Ok(None);
        }

        // Candidate blocks: (first_p, first_k) <= (partition_id, user_key) <= (last_p, last_k)
        let start_idx = self.block_indexes.partition_point(|b| {
            (b.last_partition_id, b.last_user_key.as_slice()) < (partition_id, user_key)
        });
        if start_idx >= self.block_indexes.len() {
            return Ok(None);
        }
        if (
            self.block_indexes[start_idx].first_partition_id,
            self.block_indexes[start_idx].first_user_key.as_slice(),
        ) > (partition_id, user_key)
        {
            return Ok(None);
        }

        let end_idx = self.block_indexes.partition_point(|b| {
            (b.first_partition_id, b.first_user_key.as_slice()) <= (partition_id, user_key)
        });

        let mut file = self.file.try_clone()?;

        for block in &self.block_indexes[start_idx..end_idx] {
            let entries = read_block_at(&mut file, block)?;
            for entry in entries {
                if entry.key.partition_id == partition_id
                    && entry.key.user_key.as_slice() == user_key
                    && entry.key.version <= snapshot
                {
                    return Ok(Some(entry));
                }
            }
        }

        Ok(None)
    }

    /// Return an iterator over all entries in the SST in ascending internal key order.
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<MemtableEntry>> + '_> {
        let file = self.file.try_clone()?;
        Ok(SstIterator {
            file,
            block_indexes: &self.block_indexes,
            current_block_idx: 0,
            current_entries: Vec::new().into_iter(),
            prev_key: None,
            errored: false,
        })
    }
}

/// Iterator yielding entries in ascending [`InternalKey`] order.
struct SstIterator<'a> {
    file: File,
    block_indexes: &'a [BlockIndexEntry],
    current_block_idx: usize,
    current_entries: std::vec::IntoIter<MemtableEntry>,
    prev_key: Option<InternalKey>,
    errored: bool,
}

impl<'a> Iterator for SstIterator<'a> {
    type Item = Result<MemtableEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored {
            return None;
        }

        loop {
            if let Some(entry) = self.current_entries.next() {
                if let Some(prev) = &self.prev_key {
                    if entry.key <= *prev {
                        self.errored = true;
                        return Some(Err(HtapError::Corruption(format!(
                            "unsorted or duplicate key across blocks: prev={prev:?}, curr={:?}",
                            entry.key
                        ))));
                    }
                }
                self.prev_key = Some(entry.key.clone());
                return Some(Ok(entry));
            }

            if self.current_block_idx >= self.block_indexes.len() {
                return None;
            }

            let block = &self.block_indexes[self.current_block_idx];
            self.current_block_idx += 1;

            match read_block_at(&mut self.file, block) {
                Ok(entries) => {
                    self.current_entries = entries.into_iter();
                }
                Err(e) => {
                    self.errored = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// Compute 64-bit FNV-1a hash over `partition_id.to_le_bytes()` then `user_key`.
fn fnv1a_64(partition_id: u64, user_key: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for b in partition_id.to_le_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    for &b in user_key {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Set $k$ bits in the bloom filter using double hashing `h1 + i*h2`.
fn bloom_set(bytes: &mut [u8], bit_count: u64, k: u8, hash: u64) {
    if bit_count == 0 {
        return;
    }
    let h1 = hash;
    let h2 = (hash >> 32).max(1);
    for i in 0..k {
        let bit_idx = (h1.wrapping_add((i as u64).wrapping_mul(h2))) % bit_count;
        let byte_pos = (bit_idx / 8) as usize;
        let bit_pos = (bit_idx % 8) as u8;
        if byte_pos < bytes.len() {
            bytes[byte_pos] |= 1 << bit_pos;
        }
    }
}

/// Test $k$ bits in the bloom filter using double hashing `h1 + i*h2`.
fn bloom_check(bytes: &[u8], bit_count: u64, k: u8, hash: u64) -> bool {
    if bit_count == 0 || bytes.is_empty() {
        return false;
    }
    let h1 = hash;
    let h2 = (hash >> 32).max(1);
    for i in 0..k {
        let bit_idx = (h1.wrapping_add((i as u64).wrapping_mul(h2))) % bit_count;
        let byte_pos = (bit_idx / 8) as usize;
        let bit_pos = (bit_idx % 8) as u8;
        if byte_pos >= bytes.len() || (bytes[byte_pos] & (1 << bit_pos)) == 0 {
            return false;
        }
    }
    true
}

/// Encode a single memtable entry into bytes.
fn encode_entry(entry: &MemtableEntry) -> Result<Vec<u8>> {
    let (kind, row_json) = match &entry.value {
        ValueKind::Put(row) => {
            let json = serde_json::to_vec(row)
                .map_err(|e| HtapError::Internal(format!("serializing row JSON: {e}")))?;
            if json.len() as u64 > MAX_ROW_JSON_BYTES as u64 {
                return Err(HtapError::InvalidArgument(format!(
                    "row JSON length {} exceeds limit {}",
                    json.len(),
                    MAX_ROW_JSON_BYTES
                )));
            }
            (0u8, json)
        }
        ValueKind::Delete => (1u8, Vec::new()),
    };

    if entry.key.user_key.len() as u64 > MAX_USER_KEY_BYTES as u64 {
        return Err(HtapError::InvalidArgument(format!(
            "user key length {} exceeds limit {}",
            entry.key.user_key.len(),
            MAX_USER_KEY_BYTES
        )));
    }

    let mut buf = Vec::with_capacity(8 + 4 + entry.key.user_key.len() + 8 + 1 + 4 + row_json.len());
    buf.extend_from_slice(&entry.key.partition_id.to_le_bytes());
    buf.extend_from_slice(&(entry.key.user_key.len() as u32).to_le_bytes());
    buf.extend_from_slice(&entry.key.user_key);
    buf.extend_from_slice(&entry.key.version.get().to_le_bytes());
    buf.push(kind);
    buf.extend_from_slice(&(row_json.len() as u32).to_le_bytes());
    buf.extend_from_slice(&row_json);
    Ok(buf)
}

/// Decode a single entry from a byte cursor.
fn decode_entry(cursor: &mut &[u8]) -> Result<MemtableEntry> {
    let input = *cursor;
    let mut reader = htap_common::bytecursor::ByteReader::new(input);
    let entry_eof = || HtapError::Corruption("unexpected end of entry buffer".into());

    let partition_id = reader.read_u64_le().map_err(|_| entry_eof())?;
    let user_key_len = reader.read_u32_le().map_err(|_| entry_eof())? as usize;

    if user_key_len as u32 > MAX_USER_KEY_BYTES {
        return Err(HtapError::Corruption(format!(
            "user key length {user_key_len} exceeds maximum {MAX_USER_KEY_BYTES}"
        )));
    }

    let user_key = reader
        .read_bytes(user_key_len)
        .map_err(|_| entry_eof())?
        .to_vec();
    let version = Version::new(reader.read_u64_le().map_err(|_| entry_eof())?);
    let kind_byte = reader.read_u8().map_err(|_| entry_eof())?;
    let row_json_len = reader.read_u32_le().map_err(|_| entry_eof())? as usize;

    if row_json_len as u32 > MAX_ROW_JSON_BYTES {
        return Err(HtapError::Corruption(format!(
            "row JSON length {row_json_len} exceeds maximum {MAX_ROW_JSON_BYTES}"
        )));
    }

    let row_json = reader
        .read_bytes(row_json_len)
        .map_err(|_| HtapError::Corruption("unexpected end of entry buffer for row JSON".into()))?;

    let value = match kind_byte {
        0 => {
            let row: Row = serde_json::from_slice(row_json)
                .map_err(|e| HtapError::Corruption(format!("malformed row JSON: {e}")))?;
            ValueKind::Put(row)
        }
        1 => {
            if row_json_len != 0 {
                return Err(HtapError::Corruption(
                    "tombstone entry has non-zero row_json_len".into(),
                ));
            }
            ValueKind::Delete
        }
        other => {
            return Err(HtapError::Corruption(format!(
                "invalid entry kind tag: {other}"
            )));
        }
    };

    *cursor = &input[reader.position()..];

    Ok(MemtableEntry {
        key: InternalKey {
            partition_id,
            user_key,
            version,
        },
        value,
    })
}

/// Decode all entries from a block's payload bytes.
fn decode_block_payload(payload: &[u8]) -> Result<Vec<MemtableEntry>> {
    let mut reader = htap_common::bytecursor::ByteReader::new(payload);
    let entry_count = reader
        .read_u32_le()
        .map_err(|_| HtapError::Corruption("block payload too short for entry count".into()))?
        as usize;
    let mut cursor = &payload[reader.position()..];

    let mut entries = Vec::with_capacity(entry_count.min(65536));
    let mut prev_key: Option<InternalKey> = None;

    for _ in 0..entry_count {
        let entry = decode_entry(&mut cursor)?;
        if let Some(prev) = &prev_key {
            if entry.key <= *prev {
                return Err(HtapError::Corruption(format!(
                    "unsorted or duplicate internal key in data block: prev={prev:?}, curr={:?}",
                    entry.key
                )));
            }
        }
        prev_key = Some(entry.key.clone());
        entries.push(entry);
    }

    if !cursor.is_empty() {
        return Err(HtapError::Corruption(format!(
            "trailing {} unparsed bytes in data block payload",
            cursor.len()
        )));
    }

    Ok(entries)
}

/// Read, verify, and decode a single block from disk.
fn read_block_at(file: &mut File, block: &BlockIndexEntry) -> Result<Vec<MemtableEntry>> {
    file.seek(SeekFrom::Start(block.block_offset))?;
    let mut header = [0u8; BLOCK_FRAME_HEADER_LEN];
    file.read_exact(&mut header)?;

    let mut reader = htap_common::bytecursor::ByteReader::new(&header);
    let payload_len = reader.read_u32_le().expect("fixed-size block frame header");
    let expected_crc = reader.read_u32_le().expect("fixed-size block frame header");

    if (BLOCK_FRAME_HEADER_LEN as u32) + payload_len != block.block_frame_len {
        return Err(HtapError::Corruption(format!(
            "frame length mismatch: header claims {}, index claims {}",
            (BLOCK_FRAME_HEADER_LEN as u32) + payload_len,
            block.block_frame_len
        )));
    }
    if payload_len > MAX_BLOCK_PAYLOAD_BYTES {
        return Err(HtapError::Corruption(format!(
            "payload length {payload_len} exceeds limit {MAX_BLOCK_PAYLOAD_BYTES}"
        )));
    }

    let mut payload = vec![0u8; payload_len as usize];
    file.read_exact(&mut payload)?;

    if crc32c::crc32c(&payload) != expected_crc {
        return Err(HtapError::Corruption("data block CRC mismatch".into()));
    }

    decode_block_payload(&payload)
}

/// Write a data block frame to `file` and return its [`BlockIndexEntry`] and total frame length.
fn flush_block(
    file: &mut BufWriter<File>,
    entries: &[(MemtableEntry, Vec<u8>)],
    current_offset: u64,
) -> Result<(BlockIndexEntry, u32)> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (_, encoded) in entries {
        payload.extend_from_slice(encoded);
    }

    if payload.len() as u64 > MAX_BLOCK_PAYLOAD_BYTES as u64 {
        return Err(HtapError::InvalidArgument(format!(
            "block payload {} exceeds limit {}",
            payload.len(),
            MAX_BLOCK_PAYLOAD_BYTES
        )));
    }

    let payload_len = payload.len() as u32;
    let crc = crc32c::crc32c(&payload);
    let mut frame_header = [0u8; BLOCK_FRAME_HEADER_LEN];
    frame_header[0..4].copy_from_slice(&payload_len.to_le_bytes());
    frame_header[4..8].copy_from_slice(&crc.to_le_bytes());

    file.write_all(&frame_header)?;
    file.write_all(&payload)?;

    let frame_len = (BLOCK_FRAME_HEADER_LEN as u32) + payload_len;
    let first = &entries[0].0;
    let last = &entries.last().unwrap().0;
    let min_v = entries
        .iter()
        .map(|(e, _)| e.key.version.get())
        .min()
        .unwrap();
    let max_v = entries
        .iter()
        .map(|(e, _)| e.key.version.get())
        .max()
        .unwrap();

    let index_entry = BlockIndexEntry {
        block_offset: current_offset,
        block_frame_len: frame_len,
        entry_count: entries.len() as u32,
        first_partition_id: first.key.partition_id,
        first_user_key: first.key.user_key.clone(),
        last_partition_id: last.key.partition_id,
        last_user_key: last.key.user_key.clone(),
        min_version: min_v,
        max_version: max_v,
    };

    Ok((index_entry, frame_len))
}
