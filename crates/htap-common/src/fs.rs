//! Bounded filesystem I/O utilities.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::error::{HtapError, Result};

/// Reads an entire file into memory with exact bounding and safety guarantees.
///
/// Guarantees:
/// 1. File open and metadata check happen prior to any memory allocation.
/// 2. If the file length exceeds `max_bytes`, returns [`HtapError::Corruption`].
/// 3. Rejection of short/incomplete file contents or headers is left to caller decoders as corruption.
/// 4. Reads exactly the file length observed at metadata check time; if an [`std::io::ErrorKind::UnexpectedEof`]
///    is encountered during read, it is mapped to [`HtapError::Corruption`].
/// 5. Probes one additional byte past the expected length to detect file growth or trailing corruption.
///    If trailing bytes are present, returns [`HtapError::Corruption`].
/// 6. Ordinary I/O errors are returned as [`HtapError::Io`], preserving caller `NotFound` semantics.
pub fn read_file_exact_bounded(path: impl AsRef<Path>, max_bytes: usize) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return Err(HtapError::Io(e)),
    };

    let meta = file.metadata()?;
    let len = meta.len();

    if len > max_bytes as u64 {
        return Err(HtapError::Corruption(format!(
            "file size {len} for '{}' exceeds maximum allowed bound {max_bytes}",
            path.display()
        )));
    }

    let alloc_len = usize::try_from(len).map_err(|_| {
        HtapError::Corruption(format!(
            "file size {len} for '{}' exceeds addressable memory",
            path.display()
        ))
    })?;

    let mut buf = vec![0u8; alloc_len];
    if let Err(e) = file.read_exact(&mut buf) {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            return Err(HtapError::Corruption(format!(
                "file '{}' truncated while reading: expected {alloc_len} bytes, encountered EOF",
                path.display()
            )));
        }
        return Err(HtapError::Io(e));
    }

    // Probe one byte past metadata length for file growth or trailing corruption
    let mut probe = [0u8; 1];
    let n = file.read(&mut probe)?;
    if n > 0 {
        return Err(HtapError::Corruption(format!(
            "file '{}' grew or contains trailing data beyond observed metadata length {alloc_len}",
            path.display()
        )));
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_not_found_preserved() {
        let non_existent = Path::new("/path/to/definitely/nonexistent/file/12345");
        let err = read_file_exact_bounded(non_existent, 1024).unwrap_err();
        match err {
            HtapError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected HtapError::Io(NotFound), got {other:?}"),
        }
    }

    #[test]
    fn test_normal_read_within_bound() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"hello bounded world").unwrap();
        tmp.flush().unwrap();

        let bytes = read_file_exact_bounded(tmp.path(), 64).unwrap();
        assert_eq!(bytes, b"hello bounded world");
    }

    #[test]
    fn test_exact_bound_matches_size() {
        let mut tmp = NamedTempFile::new().unwrap();
        let payload = b"exact size 16 b!";
        assert_eq!(payload.len(), 16);
        tmp.write_all(payload).unwrap();
        tmp.flush().unwrap();

        let bytes = read_file_exact_bounded(tmp.path(), 16).unwrap();
        assert_eq!(bytes, payload);
    }

    #[test]
    fn test_oversized_file_rejected_as_corruption() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"1234567890").unwrap();
        tmp.flush().unwrap();

        let err = read_file_exact_bounded(tmp.path(), 5).unwrap_err();
        assert!(matches!(err, HtapError::Corruption(_)));
        assert!(err.to_string().contains("exceeds maximum allowed bound"));
    }

    #[test]
    fn test_empty_file_returns_empty_vec() {
        let tmp = NamedTempFile::new().unwrap();
        let bytes = read_file_exact_bounded(tmp.path(), 1024).unwrap();
        assert!(bytes.is_empty());
    }
}
