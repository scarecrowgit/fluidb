//! Non-durable query spill files and statement-scoped cleanup.
//!
//! This format deliberately does not use `htap_common::envelope`: spill files
//! are disposable scratch data, so they have no checksum, no fsync guarantee,
//! and no accepted-version-range contract.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use htap_common::bytecursor::{ByteReader, ByteReaderError};
use htap_common::error::{HtapError, Result};
use htap_common::types::Row;

const MAGIC: &[u8; 8] = b"HTAPSPIL";
const HEADER_LEN: usize = MAGIC.len() + 1 + 8 + 1 + 1;
const CURRENT_FORMAT_TAG: u8 = 1;
const SPILL_READER_BUFFER_BYTES: usize = 8 * 1024;

/// Logical purpose of a spill file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SpillKind {
    HashJoin = 1,
    GroupBy = 2,
    Sort = 3,
    SetOperation = 4,
    Window = 5,
}

impl SpillKind {
    fn decode(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::HashJoin),
            2 => Ok(Self::GroupBy),
            3 => Ok(Self::Sort),
            4 => Ok(Self::SetOperation),
            5 => Ok(Self::Window),
            _ => Err(corruption(format!("unknown spill kind tag {value}"))),
        }
    }
}

/// Operator that owns a spill file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum OperatorKind {
    HashJoin = 1,
    GroupBy = 2,
    Sort = 3,
    Distinct = 4,
    SetOperation = 5,
    Window = 6,
}

impl OperatorKind {
    fn decode(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::HashJoin),
            2 => Ok(Self::GroupBy),
            3 => Ok(Self::Sort),
            4 => Ok(Self::Distinct),
            5 => Ok(Self::SetOperation),
            6 => Ok(Self::Window),
            _ => Err(corruption(format!("unknown spill operator tag {value}"))),
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::HashJoin => "hash-join.spill",
            Self::GroupBy => "group-by.spill",
            Self::Sort => "sort.spill",
            Self::Distinct => "distinct.spill",
            Self::SetOperation => "set-operation.spill",
            Self::Window => "window.spill",
        }
    }
}

/// Header stored at the beginning of each spill file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpillHeader {
    pub(crate) kind: SpillKind,
    pub(crate) statement_id: u64,
    pub(crate) operator_kind: OperatorKind,
    pub(crate) format_tag: u8,
}

impl SpillHeader {
    pub(crate) const fn new(
        kind: SpillKind,
        statement_id: u64,
        operator_kind: OperatorKind,
    ) -> Self {
        Self {
            kind,
            statement_id,
            operator_kind,
            format_tag: CURRENT_FORMAT_TAG,
        }
    }
}

/// Buffered writer for length-prefixed spill rows.
pub(crate) struct SpillWriter {
    writer: BufWriter<File>,
}

impl SpillWriter {
    pub(crate) fn create(path: &Path, header: SpillHeader) -> Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(MAGIC)?;
        writer.write_all(&[header.kind as u8])?;
        writer.write_all(&header.statement_id.to_le_bytes())?;
        writer.write_all(&[header.operator_kind as u8, header.format_tag])?;
        Ok(Self { writer })
    }

    pub(crate) fn append_row(&mut self, row: &Row) -> Result<()> {
        let encoded = serde_json::to_vec(row)
            .map_err(|error| corruption(format!("failed to encode spill row: {error}")))?;
        let length = u32::try_from(encoded.len()).map_err(|_| {
            HtapError::InvalidArgument("spill row exceeds the u32 record-size limit".into())
        })?;
        self.writer.write_all(&length.to_le_bytes())?;
        self.writer.write_all(&encoded)?;
        Ok(())
    }

    /// Flushes buffered spill data so write failures are returned to the query.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

/// Reader for a spill file using a bounded buffer and one materialized row at a time.
pub(crate) struct SpillReader {
    reader: BufReader<File>,
    header_read: bool,
}

impl SpillReader {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(SPILL_READER_BUFFER_BYTES, File::open(path)?),
            header_read: false,
        })
    }

    pub(crate) fn read_header(&mut self) -> Result<SpillHeader> {
        if self.header_read {
            return Err(corruption("spill header has already been read"));
        }

        let mut bytes = [0u8; HEADER_LEN];
        self.reader.read_exact(&mut bytes).map_err(io_corruption)?;
        let mut reader = ByteReader::new(&bytes);
        let magic = reader.read_bytes(MAGIC.len()).map_err(byte_corruption)?;
        if magic != MAGIC {
            return Err(corruption("invalid spill file magic"));
        }
        let kind = SpillKind::decode(reader.read_u8().map_err(byte_corruption)?)?;
        let statement_id = reader.read_u64_le().map_err(byte_corruption)?;
        let operator_kind = OperatorKind::decode(reader.read_u8().map_err(byte_corruption)?)?;
        let format_tag = reader.read_u8().map_err(byte_corruption)?;
        if format_tag != CURRENT_FORMAT_TAG {
            return Err(corruption(format!(
                "unsupported spill format tag {format_tag}"
            )));
        }

        self.header_read = true;
        Ok(SpillHeader {
            kind,
            statement_id,
            operator_kind,
            format_tag,
        })
    }

    pub(crate) fn read_row(&mut self) -> Result<Option<Row>> {
        if !self.header_read {
            return Err(corruption("spill header must be read before rows"));
        }

        let mut first_length_byte = [0u8; 1];
        match self.reader.read(&mut first_length_byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(error) => return Err(io_corruption(error)),
        }

        let mut length_bytes = [0u8; 4];
        length_bytes[0] = first_length_byte[0];
        self.reader
            .read_exact(&mut length_bytes[1..])
            .map_err(io_corruption)?;
        let length = usize::try_from(u32::from_le_bytes(length_bytes))
            .map_err(|_| corruption("spill record length does not fit in memory"))?;

        let mut encoded = vec![0; length];
        self.reader
            .read_exact(&mut encoded)
            .map_err(io_corruption)?;
        serde_json::from_slice(&encoded)
            .map(Some)
            .map_err(|error| corruption(format!("failed to decode spill row: {error}")))
    }
}

/// Statement-scoped spill directory that removes its tracked files on drop.
pub(crate) struct SpillDir {
    path: PathBuf,
    files: Vec<PathBuf>,
}

impl SpillDir {
    pub(crate) fn create(data_root: &Path, statement_id: u64) -> Result<Self> {
        let path = data_root.join("spill").join(statement_id.to_string());
        std::fs::create_dir_all(&path)?;
        Ok(Self {
            path,
            files: Vec::new(),
        })
    }

    pub(crate) fn partition_file_path(
        &mut self,
        operator_kind: OperatorKind,
        side: &str,
        partition: usize,
    ) -> PathBuf {
        let path = self.path.join(format!(
            "{}-{side}-{partition}.spill",
            operator_kind
                .file_name()
                .strip_suffix(".spill")
                .unwrap_or(operator_kind.file_name())
        ));
        if !self.files.contains(&path) {
            self.files.push(path.clone());
        }
        path
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        for path in &self.files {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("failed to remove spill file {}: {error}", path.display());
                }
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "failed to remove spill directory {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

fn byte_corruption(error: ByteReaderError) -> HtapError {
    corruption(format!("invalid spill framing: {error:?}"))
}

fn io_corruption(error: std::io::Error) -> HtapError {
    corruption(format!("invalid spill framing: {error}"))
}

fn corruption(message: impl Into<String>) -> HtapError {
    HtapError::Corruption(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use htap_common::types::Value;
    use tempfile::TempDir;

    fn test_header() -> SpillHeader {
        SpillHeader::new(SpillKind::HashJoin, 42, OperatorKind::HashJoin)
    }

    #[test]
    fn spill_rows_round_trip() {
        let root = TempDir::new().unwrap();
        let mut dir = SpillDir::create(root.path(), 42).unwrap();
        let path = dir.partition_file_path(OperatorKind::HashJoin, "build", 0);
        let expected = vec![
            Row::new(vec![Value::Int64(1), Value::String("one".into())]),
            Row::new(vec![Value::Null, Value::Bytes(vec![1, 2, 3])]),
        ];

        {
            let mut writer = SpillWriter::create(&path, test_header()).unwrap();
            for row in &expected {
                writer.append_row(row).unwrap();
            }
        }

        let mut reader = SpillReader::open(&path).unwrap();
        assert_eq!(reader.read_header().unwrap(), test_header());
        let mut actual = Vec::new();
        while let Some(row) = reader.read_row().unwrap() {
            actual.push(row);
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn truncated_header_and_record_are_corruption() {
        let root = TempDir::new().unwrap();
        let header_path = root.path().join("short.spill");
        std::fs::write(&header_path, &MAGIC[..3]).unwrap();

        let mut reader = SpillReader::open(&header_path).unwrap();
        assert!(matches!(
            reader.read_header(),
            Err(HtapError::Corruption(_))
        ));

        let record_path = root.path().join("record.spill");
        {
            let mut writer = SpillWriter::create(&record_path, test_header()).unwrap();
            writer.append_row(&Row::new(vec![Value::Int64(1)])).unwrap();
        }
        let mut bytes = std::fs::read(&record_path).unwrap();
        bytes.pop();
        std::fs::write(&record_path, bytes).unwrap();

        let mut reader = SpillReader::open(&record_path).unwrap();
        reader.read_header().unwrap();
        assert!(matches!(reader.read_row(), Err(HtapError::Corruption(_))));
    }

    #[test]
    fn corrupt_large_length_is_rejected_without_payload_allocation() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("large-length.spill");
        {
            let mut writer = SpillWriter::create(&path, test_header()).unwrap();
            writer.writer.write_all(&u32::MAX.to_le_bytes()).unwrap();
        }

        let mut reader = SpillReader::open(&path).unwrap();
        reader.read_header().unwrap();
        assert!(matches!(reader.read_row(), Err(HtapError::Corruption(_))));
    }

    #[test]
    fn spill_reader_uses_a_bounded_buffer() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("streaming.spill");
        {
            let mut writer = SpillWriter::create(&path, test_header()).unwrap();
            for value in 0..1_000 {
                writer
                    .append_row(&Row::new(vec![
                        Value::String("x".repeat(128)),
                        Value::Int64(value),
                    ]))
                    .unwrap();
            }
            writer.finish().unwrap();
        }

        let mut reader = SpillReader::open(&path).unwrap();
        assert_eq!(reader.reader.capacity(), SPILL_READER_BUFFER_BYTES);
        reader.read_header().unwrap();
        let mut count = 0;
        while reader.read_row().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 1_000);
    }

    #[test]
    fn spill_dir_removes_tracked_files_on_drop() {
        let root = TempDir::new().unwrap();
        let statement_path = root.path().join("spill").join("42");
        let file_path;
        {
            let mut dir = SpillDir::create(root.path(), 42).unwrap();
            file_path = dir.partition_file_path(OperatorKind::Sort, "input", 0);
            std::fs::write(&file_path, b"scratch").unwrap();
            assert!(file_path.exists());
        }
        assert!(!file_path.exists());
        assert!(!statement_path.exists());
    }
}
