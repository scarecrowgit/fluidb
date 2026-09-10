//! Process-exclusive advisory locking for storage root directories.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::error::{HtapError, Result};

/// Lock file name used for exclusive directory root ownership.
pub const LOCK_FILE_NAME: &str = "LOCK";

/// Advisory process-exclusive lock guard for database and coordinator root directories.
///
/// Ensures that at most one operating system process opens a given storage root directory.
/// The lock is backed by an OS-level non-blocking exclusive advisory lock (`flock`).
#[derive(Debug)]
pub struct ProcessLock {
    path: PathBuf,
    file: File,
}

impl ProcessLock {
    /// Acquire an exclusive, non-blocking lock on `<root>/LOCK`.
    ///
    /// The root directory must exist before calling. The lock file is created if it does not exist.
    /// If another process or file descriptor holds an exclusive lock on this file, returns [`HtapError::Conflict`].
    pub fn acquire(root: &Path) -> Result<Self> {
        let lock_path = root.join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(err) => {
                if is_contention_error(&err) {
                    let diagnostic = std::fs::read_to_string(&lock_path).unwrap_or_default();
                    let diag_trimmed = diagnostic.trim();
                    let details = if !diag_trimmed.is_empty() {
                        format!(" (active lock holder: {})", diag_trimmed)
                    } else {
                        String::new()
                    };
                    return Err(HtapError::Conflict(format!(
                        "exclusive root lock contention on '{}'{}: only one owner process may open a server or coordinator root",
                        root.display(),
                        details
                    )));
                }
                return Err(HtapError::Io(err));
            }
        }

        // Record diagnostic metadata into the lock file. Held OS lock remains sole authority.
        let pid = std::process::id();
        let now_epoch_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "pid={pid};start_time={now_epoch_secs}");
        let _ = file.flush();

        Ok(Self {
            path: lock_path,
            file,
        })
    }

    /// Return the path to the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn is_contention_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    if let Some(code) = err.raw_os_error() {
        // 11 = EAGAIN/EWOULDBLOCK on Linux/MIPS, 35 = EAGAIN on macOS/BSD, 33 = ERROR_LOCK_VIOLATION on Windows
        if code == 11 || code == 35 || code == 33 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_acquire_and_release() {
        let dir = tempfile::TempDir::new().unwrap();
        let lock1 = ProcessLock::acquire(dir.path()).expect("first lock acquire should succeed");
        assert!(lock1.path().is_file());

        // Second acquire while lock1 is held must fail with Conflict
        let err = ProcessLock::acquire(dir.path()).expect_err("second acquire must fail");
        assert!(matches!(err, HtapError::Conflict(_)));
        assert!(err.to_string().contains("contention"));

        // Dropping lock1 releases OS lock
        drop(lock1);

        // Third acquire should succeed
        let lock2 = ProcessLock::acquire(dir.path()).expect("acquire after drop should succeed");
        drop(lock2);
    }
}
