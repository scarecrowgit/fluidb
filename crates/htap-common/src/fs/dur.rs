#![allow(clippy::disallowed_methods)]
//! Filesystem operations that can be recorded by the crash-consistency simulator.
//!
//! `DurFile` and `DurOpenOptions` preserve the corresponding `std::fs` behavior.
//! With the `crashsim` feature disabled, they contain only the standard library
//! handle/options and delegate through `#[inline]` methods. With it enabled, the
//! private recorder hook boundary records successful operations only when the
//! recorder recognizes the affected path as being below a registered root.
//!
//! Sync-site identifiers use separate methods: [`DurFile::sync_all_site`] and
//! [`DurFile::sync_data_site`]. Their non-site counterparts record no site id.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// A file handle whose successful writes and syncs can be observed by crashsim.
pub struct DurFile {
    file: File,
    #[cfg(feature = "crashsim")]
    recording: Option<recording::FileHandle>,
}

impl std::fmt::Debug for DurFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurFile")
            .field("file", &self.file)
            .finish()
    }
}

impl DurFile {
    /// Wraps an already-open standard file handle.
    ///
    /// The handle has no path provenance, so crashsim cannot record operations
    /// performed through it. Use [`DurOpenOptions`] for instrumented I/O.
    #[inline]
    pub fn from_std(file: File) -> Self {
        Self {
            file,
            #[cfg(feature = "crashsim")]
            recording: None,
        }
    }

    /// Returns the underlying standard file handle.
    ///
    /// Writes performed through the returned handle bypass crash recording and
    /// cause crashsim self-checks to fail.
    #[inline]
    pub fn into_std(self) -> File {
        self.file
    }

    /// Borrows the underlying file for advisory locking and read-only metadata operations.
    ///
    /// Writes performed through this handle bypass crash recording and cause
    /// crashsim self-checks to fail.
    #[inline]
    pub fn as_std(&self) -> &File {
        &self.file
    }

    #[inline]
    fn opened(
        file: File,
        _path: &Path,
        _append: bool,
        _truncate_existing: bool,
        _create: bool,
        _existed_before_open: bool,
    ) -> Self {
        #[cfg(feature = "crashsim")]
        let recording = recording::open_file(
            &file,
            _path,
            _append,
            _truncate_existing,
            _create,
            _existed_before_open,
        );

        Self {
            file,
            #[cfg(feature = "crashsim")]
            recording,
        }
    }

    /// Synchronizes file contents and metadata.
    #[inline]
    pub fn sync_all(&self) -> io::Result<()> {
        self.sync_all_inner(None)
    }

    /// Synchronizes file contents and metadata, recording `site` in crashsim.
    #[inline]
    pub fn sync_all_site(&self, site: &'static str) -> io::Result<()> {
        self.sync_all_inner(Some(site))
    }

    #[inline]
    fn sync_all_inner(&self, _site: Option<&'static str>) -> io::Result<()> {
        self.file.sync_all()?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            recording::sync_file(handle, true, _site);
        }
        Ok(())
    }

    /// Synchronizes file contents.
    #[inline]
    pub fn sync_data(&self) -> io::Result<()> {
        self.sync_data_inner(None)
    }

    /// Synchronizes file contents, recording `site` in crashsim.
    #[inline]
    pub fn sync_data_site(&self, site: &'static str) -> io::Result<()> {
        self.sync_data_inner(Some(site))
    }

    #[inline]
    fn sync_data_inner(&self, _site: Option<&'static str>) -> io::Result<()> {
        self.file.sync_data()?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            recording::sync_file(handle, false, _site);
        }
        Ok(())
    }

    /// Changes this file's length.
    #[inline]
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        self.file.set_len(size)?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            recording::set_len(handle, size);
        }
        Ok(())
    }

    /// Returns this file's metadata.
    #[inline]
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }
}

impl Read for DurFile {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read(buf)?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            recording::read_file(handle, read);
        }
        Ok(read)
    }

    #[inline]
    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        let read = self.file.read_vectored(bufs)?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            recording::read_file(handle, read);
        }
        Ok(read)
    }
}

impl Write for DurFile {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.file.write(buf)?;
        #[cfg(feature = "crashsim")]
        if written != 0 {
            if let Some(handle) = &self.recording {
                // The recorder supplies O_APPEND offsets from its modeled length.
                recording::write_file(handle, &buf[..written]);
            }
        }
        Ok(written)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let written = self.file.write_vectored(bufs)?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            let mut remaining = written;
            for buf in bufs {
                if remaining == 0 {
                    break;
                }
                let count = remaining.min(buf.len());
                if count == 0 {
                    continue;
                }
                recording::write_file(handle, &buf[..count]);
                remaining -= count;
            }
        }
        Ok(written)
    }
}

impl Seek for DurFile {
    #[inline]
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Use the resolved absolute cursor so every SeekFrom variant is handled uniformly.
        let offset = self.file.seek(pos)?;
        #[cfg(feature = "crashsim")]
        if let Some(handle) = &self.recording {
            recording::seek_file(handle, offset);
        }
        Ok(offset)
    }
}

/// Builder for opening [`DurFile`] handles.
pub struct DurOpenOptions {
    options: OpenOptions,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl DurOpenOptions {
    #[inline]
    pub fn new() -> Self {
        Self {
            options: OpenOptions::new(),
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    #[inline]
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.options.read(read);
        self
    }

    #[inline]
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.options.write(write);
        self
    }

    #[inline]
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.options.append(append);
        self.append = append;
        self
    }

    #[inline]
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.options.truncate(truncate);
        self.truncate = truncate;
        self
    }

    #[inline]
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.options.create(create);
        self.create = create;
        self
    }

    #[inline]
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.options.create_new(create_new);
        self.create_new = create_new;
        self
    }

    #[cfg(unix)]
    #[inline]
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;

        self.options.mode(mode);
        self
    }

    #[inline]
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<DurFile> {
        let path = path.as_ref();
        #[cfg(feature = "crashsim")]
        let existed_before_open = match fs::metadata(path) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        #[cfg(not(feature = "crashsim"))]
        let existed_before_open = false;
        let truncate_existing = self.truncate && existed_before_open;
        let file = self.options.open(path)?;
        Ok(DurFile::opened(
            file,
            path,
            self.append,
            truncate_existing,
            self.create || self.create_new,
            existed_before_open,
        ))
    }
}

impl Default for DurOpenOptions {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Renames a filesystem entry.
#[inline]
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();
    #[cfg(feature = "crashsim")]
    recording::check_rename(from, to);
    fs::rename(from, to)?;
    #[cfg(feature = "crashsim")]
    recording::rename(from, to);
    Ok(())
}

/// Removes a file or symlink.
#[inline]
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    fs::remove_file(path)?;
    #[cfg(feature = "crashsim")]
    recording::remove_file(path);
    Ok(())
}

/// Creates one directory.
#[inline]
pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    fs::create_dir(path)?;
    #[cfg(feature = "crashsim")]
    recording::mkdir(path);
    Ok(())
}

/// Recursively creates directories, recording each level created by this call.
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();

    #[cfg(feature = "crashsim")]
    let missing = {
        let mut missing = Vec::new();
        let mut cursor = path;

        while !cursor.as_os_str().is_empty() {
            match fs::metadata(cursor) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing.push(cursor.to_path_buf());
                    cursor = match cursor.parent() {
                        Some(parent) => parent,
                        None => break,
                    };
                }
                Err(error) => return Err(error),
            }
        }

        missing
    };

    let result = fs::create_dir_all(path);

    #[cfg(feature = "crashsim")]
    for created in missing.iter().rev() {
        if let Ok(metadata) = fs::metadata(created) {
            if metadata.is_dir() {
                recording::mkdir(created);
            }
        }
    }

    result
}

/// Removes an empty directory.
#[inline]
pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    fs::remove_dir(path)?;
    #[cfg(feature = "crashsim")]
    recording::remove_dir(path);
    Ok(())
}

/// Recursively removes a directory, logging every unlink and directory removal.
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();

    #[cfg(feature = "crashsim")]
    if recording::is_registered(path) {
        return remove_dir_all_inner(path);
    }

    fs::remove_dir_all(path)
}

#[cfg(feature = "crashsim")]
fn remove_dir_all_inner(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "path is not a directory",
        ));
    }

    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let child = entry.path();
        let child_metadata = fs::symlink_metadata(&child)?;
        if child_metadata.file_type().is_dir() && !child_metadata.file_type().is_symlink() {
            remove_dir_all_inner(&child)?;
        } else {
            remove_file(&child)?;
        }
    }

    remove_dir(path)
}

/// Synchronizes a directory's metadata.
#[inline]
pub fn sync_dir(path: impl AsRef<Path>) -> io::Result<()> {
    sync_dir_site_inner(path.as_ref(), None)
}

/// Synchronizes a directory's metadata and records `site` in crashsim.
#[inline]
pub fn sync_dir_site(path: impl AsRef<Path>, site: &'static str) -> io::Result<()> {
    sync_dir_site_inner(path.as_ref(), Some(site))
}

#[inline]
fn sync_dir_site_inner(path: &Path, _site: Option<&'static str>) -> io::Result<()> {
    #[cfg(unix)]
    {
        let file = File::open(path)?;
        file.sync_all()?;
        #[cfg(feature = "crashsim")]
        recording::sync_dir(path, _site);
    }

    #[cfg(not(unix))]
    let _ = (path, _site);

    Ok(())
}

/// Opens `path` read-only and synchronizes its contents and metadata.
#[inline]
pub fn fsync_path(path: impl AsRef<Path>) -> io::Result<()> {
    fsync_path_site_inner(path.as_ref(), None)
}

/// Opens `path` read-only, synchronizes it, and records `site` in crashsim.
#[inline]
pub fn fsync_path_site(path: impl AsRef<Path>, site: &'static str) -> io::Result<()> {
    fsync_path_site_inner(path.as_ref(), Some(site))
}

#[inline]
fn fsync_path_site_inner(path: &Path, _site: Option<&'static str>) -> io::Result<()> {
    let file = File::open(path)?;
    file.sync_all()?;
    #[cfg(feature = "crashsim")]
    recording::sync_path(path, _site);
    Ok(())
}

#[cfg(feature = "crashsim")]
mod recording {
    pub use super::super::recorder::*;
}
