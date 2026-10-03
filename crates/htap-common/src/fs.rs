//! Bounded filesystem I/O and durable publication utilities.
//!
//! Durable helpers use crashsim sync-site identifiers to distinguish their
//! persistence barriers: `create_dir_all_durable:parent_sync`,
//! `sync_dir:sync`, `write_new_tmp_file:sync`, `fsync_file:sync`, and
//! `atomic_publish:dir_sync`.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{HtapError, Result};

#[cfg(feature = "crashsim")]
pub(crate) mod recorder;

#[cfg(feature = "crashsim")]
pub use recorder::{register, FileId, Op, Recorder, SkipSync, Snapshot};

pub mod dur;

pub use dur::{DurFile, DurOpenOptions};

/// Whether this build includes the crash-consistency simulator.
///
/// Production binaries should refuse to start when this is true.
pub const CRASHSIM_ENABLED: bool = cfg!(feature = "crashsim");

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

/// Returns `path`'s parent, mapping empty or bare paths to the current directory.
fn parent_or_current_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Fsync a directory so metadata operations such as rename are durable.
///
/// Non-Unix platforms preserve the repository's existing no-op behavior.
pub fn sync_dir(path: impl AsRef<Path>) -> Result<()> {
    dur::sync_dir_site(path, "sync_dir:sync")?;
    Ok(())
}

/// Creates a directory tree and synchronizes each parent directory whose child
/// was newly created.
///
/// Used by persistent component open paths, including the rowstore, journal,
/// catalog, and coordinator. It determines the missing path levels before
/// creating them, then syncs only the parent of each level created by this call.
pub fn create_dir_all_durable(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut cursor = path;

    // Capture only levels this invocation will create, so existing trees cause
    // no unnecessary durability barriers.
    while !cursor.as_os_str().is_empty() {
        match fs::metadata(cursor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(cursor.to_path_buf());
                cursor = match cursor.parent() {
                    Some(parent) => parent,
                    None => break,
                };
            }
            Err(error) => return Err(HtapError::Io(error)),
        }
    }

    dur::create_dir_all(path)?;

    for created in missing.iter().rev() {
        dur::sync_dir_site(
            parent_or_current_dir(created),
            "create_dir_all_durable:parent_sync",
        )?;
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
    let mut options = dur::DurOpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    if let Some(mode) = unix_mode {
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = unix_mode;

    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all_site("write_new_tmp_file:sync")?;
    Ok(())
}

/// Remove a file, treating an absent file as success.
pub fn remove_file_if_exists(path: impl AsRef<Path>) -> Result<()> {
    match dur::remove_file(path.as_ref()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(HtapError::Io(e)),
    }
}

/// Open and fsync an existing file.
pub fn fsync_file(path: impl AsRef<Path>) -> Result<()> {
    dur::fsync_path_site(path, "fsync_file:sync")?;
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
    let publish_dir = parent_or_current_dir(&tmp_path);

    if let Err(e) = write_new_tmp_file(&tmp_path, bytes, unix_mode) {
        if cleanup_tmp_on_failure {
            let _ = dur::remove_file(&tmp_path);
        }
        return Err(e);
    }

    if let Err(e) = dur::rename(&tmp_path, &final_path) {
        if cleanup_tmp_on_failure {
            let _ = dur::remove_file(&tmp_path);
        }
        return Err(HtapError::Io(e));
    }

    dur::sync_dir_site(publish_dir, "atomic_publish:dir_sync")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn parent_or_current_dir_maps_empty_and_bare_paths() {
        assert_eq!(parent_or_current_dir(Path::new("")), Path::new("."));
        assert_eq!(parent_or_current_dir(Path::new("a")), Path::new("."));
        assert_eq!(parent_or_current_dir(Path::new("a/b")), Path::new("a"));
    }

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

    #[cfg(feature = "crashsim")]
    #[test]
    fn log_orders_ops() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("ordered.log");

        write_new_tmp_file(&path, b"one", None).unwrap();
        fsync_file(&path).unwrap();
        sync_dir(recorder.root()).unwrap();

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("ordered.log")];
        assert_eq!(snapshot.files[&file_id], b"one");
        assert_eq!(
            snapshot.log,
            vec![
                Op::Create {
                    path: PathBuf::from("ordered.log"),
                    file_id,
                },
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"one".to_vec(),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("write_new_tmp_file:sync"),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("fsync_file:sync"),
                },
                Op::FsyncDir {
                    path: PathBuf::new(),
                    site: Some("sync_dir:sync"),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn append_offsets_use_model_len() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("append.log");

        write_new_tmp_file(&path, b"abc", None).unwrap();

        let mut options = DurOpenOptions::new();
        options.write(true).append(true);
        let mut file = options.open(&path).unwrap();
        file.write_all(b"def").unwrap();
        file.sync_all_site("append_offsets_use_model_len:sync")
            .unwrap();

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("append.log")];
        assert_eq!(snapshot.files[&file_id], b"abcdef");
        assert_eq!(
            snapshot.log,
            vec![
                Op::Create {
                    path: PathBuf::from("append.log"),
                    file_id,
                },
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"abc".to_vec(),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("write_new_tmp_file:sync"),
                },
                Op::Write {
                    file_id,
                    offset: 3,
                    bytes_written: b"def".to_vec(),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("append_offsets_use_model_len:sync"),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn bufwriter_logs_at_flush() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("buffered.log");

        let mut options = DurOpenOptions::new();
        options.write(true).create(true).truncate(true);
        let file = options.open(&path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        let file_id = recorder.snapshot().names[Path::new("buffered.log")];

        writer.write_all(b"buffered").unwrap();

        assert_eq!(
            recorder.snapshot().log,
            vec![Op::Create {
                path: PathBuf::from("buffered.log"),
                file_id,
            }]
        );

        writer.flush().unwrap();
        writer
            .get_mut()
            .sync_all_site("bufwriter_logs_at_flush:sync")
            .unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"buffered");
        assert_eq!(
            snapshot.log,
            vec![
                Op::Create {
                    path: PathBuf::from("buffered.log"),
                    file_id,
                },
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"buffered".to_vec(),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("bufwriter_logs_at_flush:sync"),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn partial_write_logs_returned_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("partial.log");

        let mut options = DurOpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options.open(&path).unwrap();
        let input = b"partial";
        let written = file.write(input).unwrap();
        file.sync_all_site("partial_write_logs_returned_bytes:sync")
            .unwrap();

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("partial.log")];
        let expected = input[..written].to_vec();
        assert_eq!(snapshot.files[&file_id], expected);
        match snapshot.log.as_slice() {
            [Op::Create {
                path,
                file_id: created_id,
            }, Op::Write {
                file_id: written_id,
                offset: 0,
                bytes_written,
            }, Op::FsyncFile {
                file_id: synced_id,
                site: Some("partial_write_logs_returned_bytes:sync"),
            }] => {
                assert_eq!(path, Path::new("partial.log"));
                assert_eq!(*created_id, file_id);
                assert_eq!(*written_id, file_id);
                assert_eq!(*synced_id, file_id);
                assert_eq!(bytes_written, &expected);
            }
            other => panic!("unexpected crashsim log: {other:?}"),
        }
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn rename_over_keeps_ids() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let old = recorder.root().join("old");
        let new = recorder.root().join("new");

        write_new_tmp_file(&old, b"old", None).unwrap();
        write_new_tmp_file(&new, b"new", None).unwrap();
        let before_rename = recorder.snapshot();
        let old_id = before_rename.names[Path::new("old")];
        let new_id = before_rename.names[Path::new("new")];

        dur::rename(&old, &new).unwrap();
        sync_dir(recorder.root()).unwrap();

        let snapshot = recorder.snapshot();
        assert!(!snapshot.names.contains_key(Path::new("old")));
        assert_eq!(snapshot.names[Path::new("new")], old_id);
        assert_eq!(snapshot.files[&old_id], b"old");
        match snapshot.log.as_slice() {
            [Op::Create {
                path: old_path,
                file_id: created_old_id,
            }, Op::Write {
                file_id: written_old_id,
                offset: 0,
                bytes_written: old_bytes,
            }, Op::FsyncFile {
                file_id: synced_old_id,
                site: Some("write_new_tmp_file:sync"),
            }, Op::Create {
                path: new_path,
                file_id: created_new_id,
            }, Op::Write {
                file_id: written_new_id,
                offset: 0,
                bytes_written: new_bytes,
            }, Op::FsyncFile {
                file_id: synced_new_id,
                site: Some("write_new_tmp_file:sync"),
            }, Op::Rename {
                from,
                to,
                file_id: _,
                replaced: Some(replaced),
            }, Op::FsyncDir {
                path,
                site: Some("sync_dir:sync"),
            }] => {
                assert_eq!(old_path, Path::new("old"));
                assert_eq!(*created_old_id, old_id);
                assert_eq!(*written_old_id, old_id);
                assert_eq!(old_bytes, b"old");
                assert_eq!(*synced_old_id, old_id);
                assert_eq!(new_path, Path::new("new"));
                assert_eq!(*created_new_id, new_id);
                assert_eq!(*written_new_id, new_id);
                assert_eq!(new_bytes, b"new");
                assert_eq!(*synced_new_id, new_id);
                assert_eq!(from, Path::new("old"));
                assert_eq!(to, Path::new("new"));
                assert_eq!(*replaced, new_id);
                assert_eq!(path, Path::new(""));
            }
            other => panic!("unexpected crashsim log: {other:?}"),
        }
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn fsync_path_on_readonly_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("readonly");

        write_new_tmp_file(&path, b"stable", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("readonly")];
        recorder.take_log();

        let _readonly = File::open(&path).unwrap();
        fsync_file(&path).unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"stable");
        match snapshot.log.as_slice() {
            [Op::FsyncFile {
                file_id: synced_id,
                site: Some("fsync_file:sync"),
            }] => assert_eq!(*synced_id, file_id),
            other => panic!("unexpected crashsim log: {other:?}"),
        }
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn shadow_tree_equals_real_tree() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let nested = recorder.root().join("a").join("b");

        create_dir_all_durable(&nested).unwrap();
        write_new_tmp_file(nested.join("file"), b"tree", None).unwrap();
        sync_dir(&nested).unwrap();

        let snapshot = recorder.snapshot();
        let file_path = Path::new("a").join("b").join("file");
        let file_id = snapshot.names[&file_path];
        assert!(snapshot.directories.contains(Path::new("a")));
        assert!(snapshot.directories.contains(&Path::new("a").join("b")));
        assert_eq!(snapshot.files[&file_id], b"tree");
        match snapshot.log.as_slice() {
            [Op::Mkdir { path: a }, Op::Mkdir { path: b }, Op::FsyncDir {
                path: root,
                site: Some("create_dir_all_durable:parent_sync"),
            }, Op::FsyncDir {
                path: a_parent,
                site: Some("create_dir_all_durable:parent_sync"),
            }, Op::Create {
                path: file,
                file_id: created_id,
            }, Op::Write {
                file_id: written_id,
                offset: 0,
                bytes_written,
            }, Op::FsyncFile {
                file_id: synced_id,
                site: Some("write_new_tmp_file:sync"),
            }, Op::FsyncDir {
                path: synced_dir,
                site: Some("sync_dir:sync"),
            }] => {
                assert_eq!(a, Path::new("a"));
                assert_eq!(b, &Path::new("a").join("b"));
                assert_eq!(root, Path::new(""));
                assert_eq!(a_parent, Path::new("a"));
                assert_eq!(file, &file_path);
                assert_eq!(*created_id, file_id);
                assert_eq!(*written_id, file_id);
                assert_eq!(bytes_written, b"tree");
                assert_eq!(*synced_id, file_id);
                assert_eq!(synced_dir, &Path::new("a").join("b"));
            }
            other => panic!("unexpected crashsim log: {other:?}"),
        }
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    #[should_panic(expected = "raw std::fs mutation detected")]
    fn raw_bypass_write_panics_on_selfcheck() {
        let dir = tempfile::tempdir().unwrap();
        let _recorder = register(dir.path()).unwrap();
        let path = dir.path().join("bypass");

        write_new_tmp_file(&path, b"tracked", None).unwrap();

        // Deliberately bypass the durability shim so its self-check detects drift.
        fs::write(&path, b"bypassed").unwrap();

        fsync_file(&path).unwrap();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn remove_dir_all_decomposes_into_unlinks() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let tree = recorder.root().join("tree");
        let child = tree.join("child");
        let file = child.join("file");

        create_dir_all_durable(&child).unwrap();
        write_new_tmp_file(&file, b"gone", None).unwrap();

        let tree_path = PathBuf::from("tree");
        let child_path = tree_path.join("child");
        let file_path = child_path.join("file");

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[&file_path];
        recorder.take_log();

        dur::remove_dir_all(&tree).unwrap();
        sync_dir(recorder.root()).unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot.log,
            vec![
                Op::Unlink {
                    path: file_path.clone(),
                    file_id,
                },
                Op::Rmdir {
                    path: child_path.clone(),
                },
                Op::Rmdir {
                    path: tree_path.clone(),
                },
                Op::FsyncDir {
                    path: PathBuf::new(),
                    site: Some("sync_dir:sync"),
                },
            ]
        );
        assert!(!snapshot.names.contains_key(&file_path));
        assert!(!snapshot.directories.contains(&child_path));
        assert!(!snapshot.directories.contains(&tree_path));
        assert!(!tree.exists());
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn ignored_paths_skip_sync_recording() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let spill = recorder.root().join("spill");
        let file = spill.join("file");

        create_dir_all_durable(&spill).unwrap();
        write_new_tmp_file(&file, b"ignored", None).unwrap();
        recorder.take_log();

        sync_dir(&spill).unwrap();
        fsync_file(&file).unwrap();

        let snapshot = recorder.snapshot();
        assert!(!snapshot
            .log
            .iter()
            .any(|op| matches!(op, Op::FsyncDir { .. } | Op::FsyncFile { .. })));
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn rename_same_directory_path_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("same");

        create_dir_all_durable(&path).unwrap();
        recorder.take_log();

        dur::rename(&path, &path).unwrap();

        assert!(!recorder
            .snapshot()
            .log
            .iter()
            .any(|op| matches!(op, Op::Rename { .. })));
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn rename_to_ignored_path_panics_before_syscall() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let source = recorder.root().join("source");
        let lock = recorder.root().join("LOCK");

        write_new_tmp_file(&source, b"contents", None).unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dur::rename(&source, &lock).unwrap();
        }))
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("unsupported in crash model: rename of ignored path"),
            "unexpected panic payload: {message}"
        );
        assert!(source.exists());
        assert!(!lock.exists());
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn remove_dir_all_file_returns_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("file");

        write_new_tmp_file(&path, b"contents", None).unwrap();

        let error = dur::remove_dir_all(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert!(path.exists());
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn no_recorder_is_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passthrough");

        assert!(!recorder::is_registered(&path));

        write_new_tmp_file(&path, b"plain", None).unwrap();
        fsync_file(&path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"plain");
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn seek_then_write_logs_correct_offset() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("seek");

        write_new_tmp_file(&path, b"abcdef", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("seek")];
        recorder.take_log();

        let mut options = DurOpenOptions::new();
        options.write(true);
        let mut file = options.open(&path).unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        file.write_all(b"XY").unwrap();
        file.sync_all_site("seek_then_write_logs_correct_offset:sync")
            .unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"abXYef");
        assert_eq!(
            snapshot.log,
            vec![
                Op::Write {
                    file_id,
                    offset: 2,
                    bytes_written: b"XY".to_vec(),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("seek_then_write_logs_correct_offset:sync"),
                },
            ]
        );
        assert_eq!(fs::read(&path).unwrap(), b"abXYef");
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn truncate_on_open_resets_shadow() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("truncate");

        write_new_tmp_file(&path, b"old contents", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("truncate")];
        recorder.take_log();

        write_new_tmp_file(&path, b"new", None).unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"new");
        assert_eq!(
            snapshot.log,
            vec![
                Op::SetLen { file_id, size: 0 },
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"new".to_vec(),
                },
                Op::FsyncFile {
                    file_id,
                    site: Some("write_new_tmp_file:sync"),
                },
            ]
        );
        assert_eq!(fs::read(&path).unwrap(), b"new");
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn dir_rename_panics() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let src = recorder.root().join("src");
        let dst = recorder.root().join("dst");
        let file = src.join("child").join("file");

        create_dir_all_durable(src.join("child")).unwrap();
        write_new_tmp_file(&file, b"moved", None).unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dur::rename(&src, &dst).unwrap();
        }))
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("unsupported in crash model: directory rename"),
            "unexpected panic payload: {message}"
        );
        assert!(src.exists());
        assert!(file.exists());
        assert!(!dst.exists());
        assert_eq!(fs::read(file).unwrap(), b"moved");
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn create_dir_all_partial_failure_logs_existing_levels() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let blocker = root.join("first").join("blocker");

        // Establish the pre-existing tree before crashsim captures its baseline.
        fs::create_dir_all(root.join("first")).unwrap();
        fs::write(&blocker, b"not a directory").unwrap();

        let recorder = register(dir.path()).unwrap();

        let err = create_dir_all_durable(blocker.join("child")).unwrap_err();
        assert!(matches!(err, HtapError::Io(_)));

        // The preflight metadata walk finds the file before creating anything.
        assert!(recorder
            .snapshot()
            .log
            .iter()
            .all(|op| !matches!(op, Op::Mkdir { .. })));
        assert!(!blocker.join("child").exists());
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn two_handles_have_independent_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("independent-cursors");

        write_new_tmp_file(&path, b"", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("independent-cursors")];
        recorder.take_log();

        let mut options = DurOpenOptions::new();
        options.write(true);
        let mut handle_a = options.open(&path).unwrap();
        let mut handle_b = options.open(&path).unwrap();

        handle_a.write_all(b"A").unwrap();
        handle_b.write_all(b"B").unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"B");
        assert_eq!(
            snapshot.log,
            vec![
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"A".to_vec(),
                },
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"B".to_vec(),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn reopen_without_seek_writes_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("reopen-at-zero");

        write_new_tmp_file(&path, b"oldold", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("reopen-at-zero")];
        recorder.take_log();

        let mut options = DurOpenOptions::new();
        options.write(true);
        let mut file = options.open(&path).unwrap();
        file.write_all(b"new").unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"newold");
        assert_eq!(
            snapshot.log,
            vec![Op::Write {
                file_id,
                offset: 0,
                bytes_written: b"new".to_vec(),
            }]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn read_then_write_advances_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("read-then-write");

        write_new_tmp_file(&path, b"abcdef", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("read-then-write")];
        recorder.take_log();

        let mut options = DurOpenOptions::new();
        options.read(true).write(true);
        let mut file = options.open(&path).unwrap();

        let mut prefix = [0u8; 3];
        file.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"abc");

        file.set_len(3).unwrap();
        file.write_all(b"XY").unwrap();

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"abcXY");
        assert_eq!(
            snapshot.log,
            vec![
                Op::SetLen { file_id, size: 3 },
                Op::Write {
                    file_id,
                    offset: 3,
                    bytes_written: b"XY".to_vec(),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(all(feature = "crashsim", unix))]
    #[test]
    fn create_dir_all_durable_midway_failure_logs_created_level() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let new = recorder.root().join("new");
        let oversized_component = "x".repeat(256);

        let error = create_dir_all_durable(new.join(oversized_component)).unwrap_err();
        assert!(matches!(error, HtapError::Io(_)));

        let snapshot = recorder.snapshot();
        if new.exists() {
            assert!(snapshot
                .log
                .iter()
                .any(|op| matches!(op, Op::Mkdir { path } if path == Path::new("new"))));
        } else {
            assert!(!snapshot.log.iter().any(|op| matches!(op, Op::Mkdir { .. })));
        }
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn create_dir_all_durable_logs_fsyncdir_per_level() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("a").join("b").join("c");

        create_dir_all_durable(&path).unwrap();

        let snapshot = recorder.snapshot();
        assert!(snapshot.directories.contains(Path::new("a")));
        assert!(snapshot.directories.contains(&Path::new("a").join("b")));
        assert!(snapshot
            .directories
            .contains(&Path::new("a").join("b").join("c")));
        assert_eq!(
            snapshot.log,
            vec![
                Op::Mkdir {
                    path: PathBuf::from("a"),
                },
                Op::Mkdir {
                    path: PathBuf::from("a").join("b"),
                },
                Op::Mkdir {
                    path: PathBuf::from("a").join("b").join("c"),
                },
                Op::FsyncDir {
                    path: PathBuf::new(),
                    site: Some("create_dir_all_durable:parent_sync"),
                },
                Op::FsyncDir {
                    path: PathBuf::from("a"),
                    site: Some("create_dir_all_durable:parent_sync"),
                },
                Op::FsyncDir {
                    path: PathBuf::from("a").join("b"),
                    site: Some("create_dir_all_durable:parent_sync"),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn cross_dir_rename_panics() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let from_dir = recorder.root().join("from");
        let to_dir = recorder.root().join("to");
        let from = from_dir.join("file");
        let to = to_dir.join("file");

        create_dir_all_durable(&from_dir).unwrap();
        create_dir_all_durable(&to_dir).unwrap();
        write_new_tmp_file(&from, b"moved", None).unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dur::rename(&from, &to).unwrap();
        }))
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("unsupported in crash model: cross-directory rename"),
            "unexpected panic payload: {message}"
        );
        assert!(from.exists());
        assert_eq!(fs::read(&from).unwrap(), b"moved");
        assert!(!to.exists());
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    #[should_panic(expected = "raw std::fs mutation detected")]
    fn unknown_nonempty_file_adoption_panics() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("untracked");

        fs::write(&path, b"untracked contents").unwrap();

        let mut options = DurOpenOptions::new();
        options.write(true);
        let _file = options.open(&path).unwrap();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    #[should_panic(expected = "raw std::fs mutation detected")]
    fn skipsync_still_runs_selfcheck() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("selfcheck");

        write_new_tmp_file(&path, b"tracked", None).unwrap();
        recorder.set_skip_sync(SkipSync::All);
        fs::write(&path, b"bypassed").unwrap();

        fsync_file(&path).unwrap();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn skipsync_omits_sync_ops() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("skipped-syncs");

        recorder.set_skip_sync(SkipSync::All);
        write_new_tmp_file(&path, b"contents", None).unwrap();
        fsync_file(&path).unwrap();
        sync_dir(recorder.root()).unwrap();

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("skipped-syncs")];
        assert_eq!(
            snapshot.log,
            vec![
                Op::Create {
                    path: PathBuf::from("skipped-syncs"),
                    file_id,
                },
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"contents".to_vec(),
                },
            ]
        );
        assert!(!snapshot
            .log
            .iter()
            .any(|op| matches!(op, Op::FsyncFile { .. } | Op::FsyncDir { .. })));
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn concurrent_recording_detected() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("concurrent");

        write_new_tmp_file(&path, b"contents", None).unwrap();

        let panic = std::thread::spawn(move || {
            fsync_file(&path).unwrap();
        })
        .join()
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("crashsim operations must be recorded from a single thread"),
            "unexpected panic payload: {message}"
        );
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn truncating_open_of_raw_created_file_panics() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("raw-created");

        fs::write(&path, b"raw contents").unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut options = DurOpenOptions::new();
            options.write(true).truncate(true);
            let _file = options.open(&path).unwrap();
        }))
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("raw std::fs mutation detected"),
            "unexpected panic payload: {message}"
        );
        assert!(path.exists());
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn write_vectored_skips_empty_slices() {
        use std::io::IoSlice;

        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("vectored");

        write_new_tmp_file(&path, b"", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("vectored")];
        recorder.take_log();

        let mut options = DurOpenOptions::new();
        options.write(true);
        let mut file = options.open(&path).unwrap();
        let bufs = [
            IoSlice::new(b""),
            IoSlice::new(b"ab"),
            IoSlice::new(b""),
            IoSlice::new(b"cd"),
        ];
        let written = file.write_vectored(&bufs).unwrap();

        assert_eq!(written, 4);
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"abcd");
        assert_eq!(
            snapshot.log,
            vec![
                Op::Write {
                    file_id,
                    offset: 0,
                    bytes_written: b"ab".to_vec(),
                },
                Op::Write {
                    file_id,
                    offset: 2,
                    bytes_written: b"cd".to_vec(),
                },
            ]
        );
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn zero_byte_write_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("zero-byte-write");

        write_new_tmp_file(&path, b"contents", None).unwrap();
        let file_id = recorder.snapshot().names[Path::new("zero-byte-write")];
        recorder.take_log();

        let mut options = DurOpenOptions::new();
        options.write(true);
        let mut file = options.open(&path).unwrap();
        assert_eq!(file.write(&[]).unwrap(), 0);

        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.files[&file_id], b"contents");
        assert!(snapshot.log.is_empty());
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn reregister_after_drop_with_open_handle() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("open-handle");

        write_new_tmp_file(&path, b"contents", None).unwrap();

        let mut options = DurOpenOptions::new();
        options.read(true);
        let open_handle = options.open(&path).unwrap();

        drop(recorder);
        drop(open_handle);

        let recorder = register(dir.path()).unwrap();
        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("open-handle")];
        assert_eq!(snapshot.files[&file_id], b"contents");
        recorder.verify_tree();
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn ack_from_other_thread_panics() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = register(dir.path()).unwrap();
        let path = recorder.root().join("cross-thread-ack");

        write_new_tmp_file(&path, b"", None).unwrap();

        let other_thread_recorder = recorder.clone();
        let panic = std::thread::spawn(move || {
            other_thread_recorder.ack("x");
        })
        .join()
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("crashsim operations must be recorded from a single thread"),
            "unexpected panic payload: {message}"
        );
    }

    #[cfg(feature = "crashsim")]
    #[test]
    fn parent_dir_component_under_root_panics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");

        fs::create_dir(&root).unwrap();
        fs::write(root.join("blocker"), b"not a directory").unwrap();

        let recorder = register(dir.path()).unwrap();
        let path = recorder
            .root()
            .join("root")
            .join("created")
            .join("..")
            .join("blocker")
            .join("child");

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            create_dir_all_durable(&path).unwrap();
        }))
        .unwrap_err();

        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("unsupported in crash model: '..' path component"),
            "unexpected panic payload: {message}"
        );

        // Rejection happens while recording, after the real syscall; the panic aborts the workload.
        let created = recorder.root().join("root").join("created");
        assert!(created.is_dir());
        assert!(recorder.snapshot().log.iter().any(
            |op| matches!(op, Op::Mkdir { path } if path == &PathBuf::from("root").join("created"))
        ));
        recorder.verify_tree();
    }
}
