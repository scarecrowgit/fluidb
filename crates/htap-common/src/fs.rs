//! Bounded filesystem I/O and durable publication utilities.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return Err(HtapError::Io(e)),
    };

    read_file_exact_bounded_from_file(file, path, max_bytes)
}

/// Reads an already-open file into memory with exact bounding and safety guarantees.
///
/// The file is rewound to position 0 before reading, so the complete contents
/// are returned regardless of the handle's initial cursor position.
pub fn read_file_exact_bounded_from_file(
    mut file: File,
    path: impl AsRef<Path>,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let path = path.as_ref();
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

    file.seek(SeekFrom::Start(0))?;

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

/// Fsync a directory so metadata operations such as rename are durable.
///
/// Non-Unix platforms preserve the repository's existing no-op behavior.
pub fn sync_dir(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Write, truncate, and fsync a temporary file.
///
/// `unix_mode` is applied when the file is opened on Unix and ignored on
/// non-Unix platforms.
pub fn write_new_tmp_file(
    path: impl AsRef<Path>,
    bytes: &[u8],
    unix_mode: Option<u32>,
) -> Result<()> {
    let path = path.as_ref();
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    if let Some(mode) = unix_mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = unix_mode;

    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Remove a file, treating an absent file as success.
pub fn remove_file_if_exists(path: impl AsRef<Path>) -> Result<()> {
    match fs::remove_file(path.as_ref()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(HtapError::Io(e)),
    }
}

/// Open and fsync an existing file.
pub fn fsync_file(path: impl AsRef<Path>) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// Atomically publish an in-memory buffer through a temporary file.
///
/// When `cleanup_tmp_on_failure` is true, both write and rename failures make
/// a best-effort attempt to remove the temporary file.
pub fn atomic_publish(
    dir: impl AsRef<Path>,
    tmp_name: &str,
    final_name: &str,
    bytes: &[u8],
    unix_mode: Option<u32>,
    cleanup_tmp_on_failure: bool,
) -> Result<()> {
    let dir = dir.as_ref();

    if tmp_name == final_name {
        return Err(HtapError::InvalidArgument(
            "temporary and final file names must be distinct".to_string(),
        ));
    }

    for (kind, name) in [("temporary", tmp_name), ("final", final_name)] {
        let path = Path::new(name);
        let mut components = path.components();
        if name.is_empty()
            || !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(HtapError::InvalidArgument(format!(
                "{kind} file name '{name}' must be a single path component"
            )));
        }
    }

    let tmp_path = dir.join(tmp_name);
    let final_path = dir.join(final_name);

    if let Err(e) = write_new_tmp_file(&tmp_path, bytes, unix_mode) {
        if cleanup_tmp_on_failure {
            let _ = fs::remove_file(&tmp_path);
        }
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, &final_path) {
        if cleanup_tmp_on_failure {
            let _ = fs::remove_file(&tmp_path);
        }
        return Err(HtapError::Io(e));
    }

    sync_dir(dir)
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
    fn test_read_from_file_rewinds_advanced_handle() {
        let mut tmp = NamedTempFile::new().unwrap();
        let payload = b"complete file contents";
        tmp.write_all(payload).unwrap();
        tmp.flush().unwrap();

        let mut file = File::open(tmp.path()).unwrap();
        let mut prefix = [0u8; 4];
        file.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"comp");

        let bytes = read_file_exact_bounded_from_file(file, tmp.path(), 64).unwrap();
        assert_eq!(bytes.len(), payload.len());
        assert_eq!(bytes, payload);
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

    #[test]
    fn test_remove_file_if_exists_handles_missing_and_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.tmp");

        remove_file_if_exists(&path).unwrap();
        fs::write(&path, b"stale").unwrap();
        remove_file_if_exists(&path).unwrap();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    fn install_unwritable_tmp_symlink(dir: &Path) {
        use std::os::unix::fs::symlink;

        let target = dir.join("target-dir");
        fs::create_dir(&target).unwrap();
        symlink(&target, dir.join("publish.tmp")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_publish_cleans_tmp_on_write_failure_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        install_unwritable_tmp_symlink(dir.path());

        assert!(
            atomic_publish(dir.path(), "publish.tmp", "published", b"bytes", None, true).is_err()
        );
        assert!(!dir.path().join("publish.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_atomic_publish_retains_tmp_on_write_failure_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        install_unwritable_tmp_symlink(dir.path());

        assert!(atomic_publish(
            dir.path(),
            "publish.tmp",
            "published",
            b"bytes",
            None,
            false
        )
        .is_err());
        assert!(dir.path().join("publish.tmp").exists());
    }

    #[test]
    fn test_atomic_publish_cleans_tmp_on_rename_failure_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("published")).unwrap();

        assert!(
            atomic_publish(dir.path(), "publish.tmp", "published", b"bytes", None, true).is_err()
        );
        assert!(!dir.path().join("publish.tmp").exists());
    }

    #[test]
    fn test_atomic_publish_retains_tmp_on_rename_failure_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("published")).unwrap();

        assert!(atomic_publish(
            dir.path(),
            "publish.tmp",
            "published",
            b"bytes",
            None,
            false
        )
        .is_err());
        assert_eq!(fs::read(dir.path().join("publish.tmp")).unwrap(), b"bytes");
    }
}
