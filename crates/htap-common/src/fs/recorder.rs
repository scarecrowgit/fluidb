//! Crash-consistency operation recorder and shadow filesystem model.
//!
//! This module is enabled only with the `crashsim` feature. Filesystem shim
//! operations call the `record_*` methods after their corresponding syscall
//! succeeds. The recorder maintains an inode-like identity model independent
//! of the names currently referring to each file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

/// An identifier that remains associated with a file across renames.
pub type FileId = u64;

/// A successfully completed filesystem mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Op {
    Mkdir {
        path: PathBuf,
    },
    Create {
        path: PathBuf,
        file_id: FileId,
    },
    Write {
        file_id: FileId,
        offset: u64,
        bytes_written: Vec<u8>,
    },
    SetLen {
        file_id: FileId,
        size: u64,
    },
    FsyncFile {
        file_id: FileId,
        site: Option<&'static str>,
    },
    FsyncDir {
        path: PathBuf,
        site: Option<&'static str>,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        /// Inode-like id of the source file moved by this rename.
        file_id: FileId,
        /// Inode-like id of the destination file replaced by this rename.
        ///
        /// Field-pattern matches in downstream crates must bind these fields or
        /// use `..`.
        replaced: Option<FileId>,
    },
    Unlink {
        path: PathBuf,
        file_id: FileId,
    },
    Rmdir {
        path: PathBuf,
    },
    Ack {
        label: String,
    },
}

/// A copyable representation of the recorder's filesystem model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// Canonical registered root represented by this snapshot.
    pub root: PathBuf,
    /// Operations retained by the recorder at snapshot time.
    pub log: Vec<Op>,
    /// Directories present at registration, relative to `root`.
    pub baseline_directories: BTreeSet<PathBuf>,
    /// Relative file paths and inode-like ids present at registration.
    pub baseline_names: BTreeMap<PathBuf, FileId>,
    /// File contents present at registration, indexed by inode-like id.
    pub baseline_files: BTreeMap<FileId, Vec<u8>>,
    /// Directories relative to `root`, including the empty root path.
    pub directories: BTreeSet<PathBuf>,
    /// Relative file paths and their inode-like ids.
    pub names: BTreeMap<PathBuf, FileId>,
    /// Contents indexed by inode-like id.
    pub files: BTreeMap<FileId, Vec<u8>>,
}

/// Controls which sync operation, if any, fails before reaching the filesystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncFault {
    /// Do not inject a sync failure.
    None,
    /// Fail the 1-based `n`th sync attempt after this fault is armed.
    Nth(u64),
    /// Fail the 1-based occurrence of a sync carrying `site`.
    Site { site: &'static str, occurrence: u64 },
}

/// Controls which otherwise-successful sync operations are omitted from the
/// crash model's operation log.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SkipSync {
    /// Record every sync operation.
    #[default]
    None,
    /// Omit all file sync operations.
    File,
    /// Omit all directory sync operations.
    Directory,
    /// Omit every sync operation.
    All,
    /// Omit sync operations with this site identifier.
    Site(&'static str),
}

impl SkipSync {
    fn skips_file(&self, site: Option<&'static str>) -> bool {
        matches!(self, Self::File | Self::All)
            || matches!(self, Self::Site(expected) if Some(*expected) == site)
    }

    fn skips_dir(&self, site: Option<&'static str>) -> bool {
        matches!(self, Self::Directory | Self::All)
            || matches!(self, Self::Site(expected) if Some(*expected) == site)
    }
}

fn should_skip_file_sync(skip_sync: &SkipSync, site: Option<&'static str>) -> bool {
    let scoped_skip_sync = super::current_skip_sync();
    if scoped_skip_sync == SkipSync::None {
        skip_sync.skips_file(site)
    } else {
        let should_skip = scoped_skip_sync.skips_file(site);
        if should_skip {
            super::record_scope_skip_hit();
        }
        should_skip
    }
}

fn should_skip_dir_sync(skip_sync: &SkipSync, site: Option<&'static str>) -> bool {
    let scoped_skip_sync = super::current_skip_sync();
    if scoped_skip_sync == SkipSync::None {
        skip_sync.skips_dir(site)
    } else {
        let should_skip = scoped_skip_sync.skips_dir(site);
        if should_skip {
            super::record_scope_skip_hit();
        }
        should_skip
    }
}

/// Handle returned by [`register`]. Dropping the final handle unregisters the
/// root and prevents subsequent operations from being recorded.
#[derive(Clone)]
pub struct Recorder {
    state: Arc<Mutex<State>>,
}

impl Recorder {
    /// Canonical registered root.
    pub fn root(&self) -> PathBuf {
        self.state.lock().root.clone()
    }

    /// Changes which successful syncs are omitted from the operation log.
    pub fn set_skip_sync(&self, skip_sync: SkipSync) {
        self.state.lock().skip_sync = skip_sync;
    }

    /// Arms sync-failure injection and resets its matching counters.
    pub fn set_sync_fault(&self, sync_fault: SyncFault) {
        assert!(
            !matches!(
                &sync_fault,
                SyncFault::Nth(0) | SyncFault::Site { occurrence: 0, .. }
            ),
            "crashsim sync fault selectors are 1-based"
        );

        let mut state = self.state.lock();
        state.sync_fault = sync_fault;
        state.sync_attempts = 0;
        state.sync_site_attempts.clear();
        state.sync_faults_fired = 0;
    }

    /// Returns the number of sync attempts since the current fault was armed.
    pub fn sync_attempts(&self) -> u64 {
        self.state.lock().sync_attempts
    }

    /// Returns the number of injected sync failures since the current fault was armed.
    pub fn sync_faults_fired(&self) -> u64 {
        self.state.lock().sync_faults_fired
    }

    /// Returns the number of sync operations omitted from the operation log.
    pub fn skip_hits(&self) -> usize {
        self.state.lock().skip_hits
    }

    /// Appends an explicit acknowledgement marker.
    pub fn ack(&self, label: impl Into<String>) {
        let mut state = self.state.lock();
        state.assert_recording_thread();
        state.log.push(Op::Ack {
            label: label.into(),
        });
    }

    /// Removes and returns all operations recorded so far.
    ///
    /// This is test-only: the baseline is not advanced, so snapshots taken
    /// after `take_log` are not replayable by htap-crashsim.
    pub fn take_log(&self) -> Vec<Op> {
        std::mem::take(&mut self.state.lock().log)
    }

    /// Returns the current shadow filesystem state and a clone of its log.
    pub fn snapshot(&self) -> Snapshot {
        let state = self.state.lock();
        Snapshot {
            root: state.root.clone(),
            log: state.log.clone(),
            baseline_directories: state.baseline_directories.clone(),
            baseline_names: state.baseline_names.clone(),
            baseline_files: state.baseline_files.clone(),
            directories: state.directories.clone(),
            names: state.names.clone(),
            files: state.files.clone(),
        }
    }

    /// Registers a data directory whose runtime artifacts are excluded from
    /// the durability model.
    ///
    /// `rel` is relative to the registered root and must not contain `..`.
    pub fn add_ephemeral_data_root(&self, rel: impl AsRef<Path>) {
        let rel = rel.as_ref();
        assert!(
            !rel.is_absolute()
                && !rel
                    .components()
                    .any(|component| component == Component::ParentDir),
            "ephemeral data root must be relative and must not contain '..': '{}'",
            rel.display()
        );
        self.state.lock().add_ephemeral_data_root(rel.to_path_buf());
    }

    /// Panics unless the modeled tree precisely matches the on-disk tree.
    ///
    /// Runtime artifacts directly beneath each ephemeral data root are
    /// ignored: `LOCK`, `spill/`, `htap.sock`, and `.htap-ipc-*` entries.
    pub fn verify_tree(&self) {
        self.state.lock().verify_tree();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) != 1 {
            return;
        }

        let root = self.state.lock().root.clone();
        registry().lock().remove(&root);
    }
}

/// Registers `root` and returns the recorder for it.
///
/// `root` must already exist so it can be canonicalized. Registering an
/// existing root, an ancestor of an existing root, or a descendant of an
/// existing root panics.
pub fn register(root: impl AsRef<Path>) -> io::Result<Recorder> {
    let root = fs::canonicalize(root)?;
    let mut roots = registry().lock();
    roots.retain(|_, state| state.strong_count() > 0);

    if roots.keys().any(|existing| {
        existing == &root || existing.starts_with(&root) || root.starts_with(existing)
    }) {
        panic!(
            "crashsim recorder roots must not overlap: attempted registration of '{}'",
            root.display()
        );
    }

    let state = Arc::new(Mutex::new(State::from_disk(root.clone())?));
    roots.insert(root, Arc::downgrade(&state));
    Ok(Recorder { state })
}

/// Returns whether `path` is under a registered root.
pub fn is_registered(path: &Path) -> bool {
    for_path(path).is_some()
}

/// A registered path resolved exactly once for both lookup and recording.
struct RegisteredPath {
    state: Arc<Mutex<State>>,
    path: PathBuf,
}

/// Returns the recorder responsible for `path`, together with its resolved path.
///
/// Resolution failures beneath a registered root fail closed. This prevents a
/// successful filesystem mutation from silently escaping the shadow model.
fn for_path(path: &Path) -> Option<RegisteredPath> {
    let unresolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };

    let mut roots = registry().lock();
    roots.retain(|_, state| state.strong_count() > 0);

    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        let resolved = absolute_normalized(path).ok();
        if roots.keys().any(|root| {
            unresolved.starts_with(root)
                || resolved
                    .as_ref()
                    .is_some_and(|resolved| resolved.starts_with(root))
        }) {
            panic!(
                "unsupported in crash model: '..' path component: '{}'",
                path.display()
            );
        }
    }

    let resolved = match absolute_normalized(path) {
        Ok(path) => path,
        Err(error) => {
            if roots.keys().any(|root| unresolved.starts_with(root)) {
                panic!(
                    "path unreachable from registered root: '{}': {error}",
                    path.display()
                );
            }
            return None;
        }
    };

    if roots
        .keys()
        .any(|root| unresolved.starts_with(root) && !resolved.starts_with(root))
    {
        panic!(
            "path escapes registered root after resolution: '{}'",
            path.display()
        );
    }

    roots.iter().find_map(|(root, state)| {
        if !resolved.starts_with(root) {
            return None;
        }

        Some(RegisteredPath {
            state: state.upgrade().unwrap_or_else(|| {
                panic!(
                    "recorder disappeared while resolving registered path '{}'",
                    path.display()
                )
            }),
            path: resolved.clone(),
        })
    })
}

/// Records a file opened through the durable shim.
pub(crate) fn open_file(
    file: &fs::File,
    path: &Path,
    append: bool,
    truncate_existing: bool,
    create: bool,
    existed_before_open: bool,
) -> Option<FileHandle> {
    let registered = for_path(path)?;
    let file_id = {
        let mut state_guard = registered.state.lock();
        let relative = state_guard.relative(&registered.path).unwrap_or_else(|| {
            panic!(
                "path resolved differently than recorded: '{}'",
                path.display()
            )
        });
        if state_guard.ignored(&relative) {
            return None;
        }

        state_guard.assert_recording_thread();
        let file_id = state_guard
            .ensure_file(file, &relative, create, existed_before_open)
            .unwrap_or_else(|| {
                panic!(
                    "failed to record file under registered root: '{}'",
                    path.display()
                )
            });
        *state_guard.open_handles.entry(file_id).or_default() += 1;
        if truncate_existing {
            state_guard.set_len(file_id, 0);
        }
        let cursor = if append {
            state_guard.files[&file_id].len() as u64
        } else {
            0
        };
        (file_id, cursor)
    };
    Some(FileHandle {
        state: registered.state,
        file_id: file_id.0,
        append,
        cursor: Mutex::new(file_id.1),
    })
}

/// Per-open durable shim state.
pub(crate) struct FileHandle {
    state: Arc<Mutex<State>>,
    file_id: FileId,
    append: bool,
    cursor: Mutex<u64>,
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        let count = state
            .open_handles
            .get_mut(&self.file_id)
            .expect("dropped unknown crashsim file handle");
        *count -= 1;
        if *count == 0 {
            state.open_handles.remove(&self.file_id);
            state.remove_file_if_closed_and_nameless(self.file_id);
        }
    }
}

pub(crate) fn read_file(handle: &FileHandle, bytes_read: usize) {
    let mut state = handle.state.lock();
    state.assert_recording_thread();
    *handle.cursor.lock() += bytes_read as u64;
}

pub(crate) fn write_file(handle: &FileHandle, bytes: &[u8]) {
    let mut state = handle.state.lock();
    state.assert_recording_thread();

    let mut cursor = handle.cursor.lock();
    let offset = if handle.append {
        state.files[&handle.file_id].len() as u64
    } else {
        *cursor
    };

    state.write(handle.file_id, offset, bytes);
    *cursor = offset + bytes.len() as u64;
}

pub(crate) fn seek_file(handle: &FileHandle, offset: u64) {
    let mut state = handle.state.lock();
    state.assert_recording_thread();
    *handle.cursor.lock() = offset;
}

pub(crate) fn set_len(handle: &FileHandle, size: u64) {
    let mut state = handle.state.lock();
    state.assert_recording_thread();
    state.set_len(handle.file_id, size);
}

pub(crate) fn pre_sync_file(
    handle: &FileHandle,
    _all: bool,
    site: Option<&'static str>,
) -> io::Result<()> {
    let mut state = handle.state.lock();
    state.assert_recording_thread();
    state.check_sync_fault(site)
}

pub(crate) fn sync_file(handle: &FileHandle, _all: bool, site: Option<&'static str>) {
    let mut state = handle.state.lock();
    state.assert_recording_thread();
    state.assert_file_matches_disk(handle.file_id);
    if should_skip_file_sync(&state.skip_sync, site) {
        state.skip_hits += 1;
        return;
    }

    state.log.push(Op::FsyncFile {
        file_id: handle.file_id,
        site,
    });
}

pub(crate) fn check_rename(from: &Path, to: &Path) {
    let from_registered = for_path(from);
    let to_registered = for_path(to);

    if from_registered.is_none() && to_registered.is_none() {
        return;
    }

    let from_path = from_registered
        .as_ref()
        .map(|registered| registered.path.clone())
        .unwrap_or_else(|| {
            absolute_normalized(from).unwrap_or_else(|error| {
                panic!(
                    "path unreachable from registered root: '{}': {error}",
                    from.display()
                )
            })
        });
    let to_path = to_registered
        .as_ref()
        .map(|registered| registered.path.clone())
        .unwrap_or_else(|| {
            absolute_normalized(to).unwrap_or_else(|error| {
                panic!(
                    "path unreachable from registered root: '{}': {error}",
                    to.display()
                )
            })
        });

    if from_path == to_path {
        return;
    }

    if from_registered.as_ref().is_some_and(|registered| {
        let state = registered.state.lock();
        state
            .relative(&registered.path)
            .is_some_and(|path| state.ignored(&path))
    }) || to_registered.as_ref().is_some_and(|registered| {
        let state = registered.state.lock();
        state
            .relative(&registered.path)
            .is_some_and(|path| state.ignored(&path))
    }) {
        panic!("unsupported in crash model: rename of ignored path");
    }

    if from_path.parent() != to_path.parent() {
        panic!("unsupported in crash model: cross-directory rename");
    }

    if fs::symlink_metadata(from).is_ok_and(|metadata| metadata.is_dir()) {
        panic!("unsupported in crash model: directory rename");
    }
}

pub(crate) fn rename(from: &Path, to: &Path) {
    let from_registered = for_path(from);
    let to_registered = for_path(to);

    let Some(registered) = from_registered.as_ref().or(to_registered.as_ref()) else {
        return;
    };

    if let (Some(from_registered), Some(to_registered)) =
        (from_registered.as_ref(), to_registered.as_ref())
    {
        if !Arc::ptr_eq(&from_registered.state, &to_registered.state) {
            panic!(
                "rename crosses registered roots: '{}' -> '{}'",
                from.display(),
                to.display()
            );
        }
    }

    let from_path = from_registered
        .as_ref()
        .map(|registered| registered.path.clone())
        .unwrap_or_else(|| {
            absolute_normalized(from).unwrap_or_else(|error| {
                panic!(
                    "path unreachable from registered root: '{}': {error}",
                    from.display()
                )
            })
        });
    let to_path = to_registered
        .as_ref()
        .map(|registered| registered.path.clone())
        .unwrap_or_else(|| {
            absolute_normalized(to).unwrap_or_else(|error| {
                panic!(
                    "path unreachable from registered root: '{}': {error}",
                    to.display()
                )
            })
        });

    let mut state = registered.state.lock();
    state.assert_recording_thread();
    state.rename(&from_path, &to_path);
}

pub(crate) fn remove_file(path: &Path) {
    let Some(registered) = for_path(path) else {
        return;
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    state.unlink(&registered.path);
}

pub(crate) fn mkdir(path: &Path) {
    let Some(registered) = for_path(path) else {
        return;
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    state.mkdir(&registered.path);
}

pub(crate) fn remove_dir(path: &Path) {
    let Some(registered) = for_path(path) else {
        return;
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    state.rmdir(&registered.path);
}

pub(crate) fn pre_sync_dir(path: &Path, site: Option<&'static str>) -> io::Result<()> {
    let Some(registered) = for_path(path) else {
        return Ok(());
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    let relative = state.relative(&registered.path).unwrap_or_else(|| {
        panic!(
            "path resolved differently than recorded: '{}'",
            path.display()
        )
    });
    if state.ignored(&relative) {
        return Ok(());
    }
    state.check_sync_fault(site)
}

pub(crate) fn sync_dir(path: &Path, site: Option<&'static str>) {
    let Some(registered) = for_path(path) else {
        return;
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    let relative = state.relative(&registered.path).unwrap_or_else(|| {
        panic!(
            "path resolved differently than recorded: '{}'",
            path.display()
        )
    });
    if state.ignored(&relative) {
        return;
    }
    if !should_skip_dir_sync(&state.skip_sync, site) {
        state.log.push(Op::FsyncDir {
            path: relative,
            site,
        });
    } else {
        state.skip_hits += 1;
    }
}

pub(crate) fn pre_sync_path(path: &Path, site: Option<&'static str>) -> io::Result<()> {
    let Some(registered) = for_path(path) else {
        return Ok(());
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    let relative = state.relative(&registered.path).unwrap_or_else(|| {
        panic!(
            "path resolved differently than recorded: '{}'",
            path.display()
        )
    });
    if state.ignored(&relative) {
        return Ok(());
    }
    state.check_sync_fault(site)
}

pub(crate) fn sync_path(path: &Path, site: Option<&'static str>) {
    let Some(registered) = for_path(path) else {
        return;
    };
    let mut state = registered.state.lock();
    state.assert_recording_thread();
    let relative = state.relative(&registered.path).unwrap_or_else(|| {
        panic!(
            "path resolved differently than recorded: '{}'",
            path.display()
        )
    });
    if state.ignored(&relative) {
        return;
    }
    let file_id = state
        .names
        .get(&relative)
        .copied()
        .unwrap_or_else(|| panic!("file is missing from crashsim model: '{}'", path.display()));
    state.assert_file_matches_disk(file_id);
    if should_skip_file_sync(&state.skip_sync, site) {
        state.skip_hits += 1;
        return;
    }
    state.log.push(Op::FsyncFile { file_id, site });
}

struct State {
    root: PathBuf,
    log: Vec<Op>,
    skip_sync: SkipSync,
    skip_hits: usize,
    sync_fault: SyncFault,
    sync_attempts: u64,
    sync_site_attempts: BTreeMap<&'static str, u64>,
    sync_faults_fired: u64,
    next_file_id: FileId,
    baseline_directories: BTreeSet<PathBuf>,
    baseline_names: BTreeMap<PathBuf, FileId>,
    baseline_files: BTreeMap<FileId, Vec<u8>>,
    ephemeral_data_roots: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
    names: BTreeMap<PathBuf, FileId>,
    files: BTreeMap<FileId, Vec<u8>>,
    open_handles: BTreeMap<FileId, usize>,
    recording_thread: Option<std::thread::ThreadId>,
}

impl State {
    fn from_disk(root: PathBuf) -> io::Result<Self> {
        let mut state = Self {
            root,
            log: Vec::new(),
            skip_sync: SkipSync::None,
            skip_hits: 0,
            sync_fault: SyncFault::None,
            sync_attempts: 0,
            sync_site_attempts: BTreeMap::new(),
            sync_faults_fired: 0,
            next_file_id: 1,
            baseline_directories: BTreeSet::new(),
            baseline_names: BTreeMap::new(),
            baseline_files: BTreeMap::new(),
            ephemeral_data_roots: BTreeSet::from([PathBuf::new()]),
            directories: BTreeSet::from([PathBuf::new()]),
            names: BTreeMap::new(),
            files: BTreeMap::new(),
            open_handles: BTreeMap::new(),
            recording_thread: None,
        };
        let root = state.root.clone();
        state.import_tree(&root, Path::new(""))?;
        state.baseline_directories = state.directories.clone();
        state.baseline_names = state.names.clone();
        state.baseline_files = state.files.clone();
        Ok(state)
    }

    fn import_tree(&mut self, absolute: &Path, relative: &Path) -> io::Result<()> {
        let mut entries = fs::read_dir(absolute)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name();
            let child_relative = relative.join(name);
            if self.ignored(&child_relative) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                self.directories.insert(child_relative.clone());
                self.import_tree(&entry.path(), &child_relative)?;
            } else if metadata.is_file() {
                let id = self.allocate_id();
                self.names.insert(child_relative, id);
                self.files.insert(id, read_all(&entry.path())?);
            }
        }
        Ok(())
    }

    fn allocate_id(&mut self) -> FileId {
        let id = self.next_file_id;
        self.next_file_id += 1;
        id
    }

    fn add_ephemeral_data_root(&mut self, relative: PathBuf) {
        assert!(
            self.log.is_empty() && self.open_handles.is_empty(),
            "crashsim ephemeral data roots must be added immediately after registration"
        );

        if !self.ephemeral_data_roots.insert(relative) {
            return;
        }

        // Registration scans disk before extra roots are known, so purge newly
        // ignored entries from the baseline and rebuild the current model.
        let ephemeral_data_roots = self.ephemeral_data_roots.clone();
        self.baseline_directories
            .retain(|path| !ignored_by(&ephemeral_data_roots, path));
        self.baseline_names
            .retain(|path, _| !ignored_by(&ephemeral_data_roots, path));

        let baseline_ids: BTreeSet<_> = self.baseline_names.values().copied().collect();
        self.baseline_files
            .retain(|file_id, _| baseline_ids.contains(file_id));

        self.directories = self.baseline_directories.clone();
        self.names = self.baseline_names.clone();
        self.files = self.baseline_files.clone();
    }

    fn ignored(&self, relative: &Path) -> bool {
        ignored_by(&self.ephemeral_data_roots, relative)
    }

    fn relative(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root).ok().map(Path::to_path_buf)
    }

    fn assert_recording_thread(&mut self) {
        let current = std::thread::current().id();
        match self.recording_thread {
            Some(recording_thread) => assert_eq!(
                recording_thread, current,
                "crashsim operations must be recorded from a single thread"
            ),
            None => self.recording_thread = Some(current),
        }
    }

    fn check_sync_fault(&mut self, site: Option<&'static str>) -> io::Result<()> {
        self.sync_attempts += 1;
        let site_occurrence = site.map(|site| {
            let occurrence = self.sync_site_attempts.entry(site).or_default();
            *occurrence += 1;
            *occurrence
        });

        let should_fail = match &self.sync_fault {
            SyncFault::None => false,
            SyncFault::Nth(attempt) => self.sync_attempts == *attempt,
            SyncFault::Site {
                site: expected,
                occurrence,
            } => site == Some(*expected) && site_occurrence == Some(*occurrence),
        };

        if should_fail {
            self.sync_faults_fired += 1;
            return Err(io::Error::other("crashsim injected sync failure"));
        }

        Ok(())
    }

    fn ensure_file(
        &mut self,
        file: &fs::File,
        relative: &Path,
        create: bool,
        existed_before_open: bool,
    ) -> Option<FileId> {
        if self.ignored(relative) {
            return None;
        }
        if let Some(id) = self.names.get(relative) {
            return Some(*id);
        }

        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            return None;
        }
        if existed_before_open {
            panic!(
                "raw std::fs mutation detected under registered root: '{}'",
                self.root.join(relative).display()
            );
        }

        // A write-only handle cannot be read back through `file`; reopen by
        // path to distinguish a newly created file from an unrecorded one.
        let contents = read_all(&self.root.join(relative)).unwrap_or_else(|error| {
            panic!(
                "failed to read existing crashsim file '{}': {error}",
                self.root.join(relative).display()
            )
        });
        if !create || !contents.is_empty() {
            panic!(
                "raw std::fs mutation detected under registered root: '{}'",
                self.root.join(relative).display()
            );
        }

        let id = self.allocate_id();
        self.names.insert(relative.to_path_buf(), id);
        self.files.insert(id, contents);
        self.log.push(Op::Create {
            path: relative.to_path_buf(),
            file_id: id,
        });
        Some(id)
    }

    fn write(&mut self, file_id: FileId, offset: u64, bytes: &[u8]) {
        let file = self
            .files
            .get_mut(&file_id)
            .expect("write through unknown crashsim file handle");
        let offset = usize::try_from(offset).expect("file offset exceeds addressable memory");
        if file.len() < offset {
            file.resize(offset, 0);
        }
        let end = offset
            .checked_add(bytes.len())
            .expect("crashsim modeled file length overflow");
        if file.len() < end {
            file.resize(end, 0);
        }
        file[offset..end].copy_from_slice(bytes);
        self.log.push(Op::Write {
            file_id,
            offset: offset as u64,
            bytes_written: bytes.to_vec(),
        });
    }

    fn set_len(&mut self, file_id: FileId, size: u64) {
        let size = usize::try_from(size).expect("file size exceeds addressable memory");
        self.files
            .get_mut(&file_id)
            .expect("set_len through unknown crashsim file handle")
            .resize(size, 0);
        self.log.push(Op::SetLen {
            file_id,
            size: size as u64,
        });
    }

    fn remove_file_if_closed_and_nameless(&mut self, file_id: FileId) {
        let is_open = self
            .open_handles
            .get(&file_id)
            .is_some_and(|count| *count > 0);
        let is_named = self.names.values().any(|id| *id == file_id);
        if !is_open && !is_named {
            self.files.remove(&file_id);
        }
    }

    fn mkdir(&mut self, path: &Path) {
        let relative = self
            .relative(path)
            .expect("created directory must be under registered root");
        if !self.ignored(&relative) {
            self.directories.insert(relative.clone());
            self.log.push(Op::Mkdir { path: relative });
        }
    }

    fn unlink(&mut self, path: &Path) {
        let relative = self
            .relative(path)
            .expect("removed file must be under registered root");
        if self.ignored(&relative) {
            return;
        }
        let id = self
            .names
            .remove(&relative)
            .expect("raw std::fs mutation detected under registered root");
        self.log.push(Op::Unlink {
            path: relative,
            file_id: id,
        });
        self.remove_file_if_closed_and_nameless(id);
    }

    fn rmdir(&mut self, path: &Path) {
        let relative = self
            .relative(path)
            .expect("removed directory must be under registered root");
        if !self.ignored(&relative) {
            if !self.directories.remove(&relative) {
                panic!("raw std::fs mutation detected under registered root");
            }
            self.log.push(Op::Rmdir { path: relative });
        }
    }

    fn rename(&mut self, from: &Path, to: &Path) {
        let from_relative = self
            .relative(from)
            .expect("rename source must be under registered root");
        let to_relative = self
            .relative(to)
            .expect("rename target must be under registered root");

        if from_relative == to_relative {
            return;
        }

        if from_relative.parent() != to_relative.parent() {
            panic!("unsupported in crash model: cross-directory rename");
        }

        if self.directories.contains(&from_relative) {
            panic!("unsupported in crash model: directory rename");
        }

        if self.ignored(&from_relative) || self.ignored(&to_relative) {
            return;
        }

        if self.directories.contains(&to_relative) {
            panic!("unsupported in crash model: cannot rename over an existing directory");
        }

        let file_id = self
            .names
            .get(&from_relative)
            .copied()
            .unwrap_or_else(|| panic!("raw std::fs mutation detected under registered root"));

        let replaced = self.names.remove(&to_relative);
        if let Some(replaced) = replaced {
            self.remove_file_if_closed_and_nameless(replaced);
        }

        if let Some(id) = self.names.remove(&from_relative) {
            self.names.insert(to_relative.clone(), id);
        } else if self.directories.contains(&from_relative) {
            let directories: Vec<_> = self
                .directories
                .range(from_relative.clone()..)
                .take_while(|path| path.starts_with(&from_relative))
                .cloned()
                .collect();
            let names: Vec<_> = self
                .names
                .range(from_relative.clone()..)
                .take_while(|(path, _)| path.starts_with(&from_relative))
                .map(|(path, id)| (path.clone(), *id))
                .collect();

            for old in directories {
                self.directories.remove(&old);
                self.directories
                    .insert(to_relative.join(old.strip_prefix(&from_relative).unwrap()));
            }
            for (old, id) in names {
                self.names.remove(&old);
                self.names.insert(
                    to_relative.join(old.strip_prefix(&from_relative).unwrap()),
                    id,
                );
            }
        } else {
            panic!("raw std::fs mutation detected under registered root");
        }

        self.log.push(Op::Rename {
            from: from_relative,
            to: to_relative,
            file_id,
            replaced,
        });
    }

    fn assert_file_matches_disk(&self, file_id: FileId) {
        let Some(relative) = self
            .names
            .iter()
            .find_map(|(path, id)| (*id == file_id).then_some(path))
        else {
            return;
        };
        let real = read_all(&self.root.join(relative))
            .expect("failed to read real file during crashsim self-check");
        assert_eq!(
            self.files[&file_id],
            real,
            "raw std::fs mutation detected under registered root: '{}'",
            relative.display()
        );
    }

    fn verify_tree(&self) {
        let mut real_dirs = BTreeSet::from([PathBuf::new()]);
        let mut real_files = BTreeMap::new();
        collect_tree(
            &self.root,
            Path::new(""),
            &self.ephemeral_data_roots,
            &mut real_dirs,
            &mut real_files,
        )
        .expect("failed to enumerate real tree during crashsim self-check");

        assert_eq!(
            self.directories, real_dirs,
            "raw std::fs directory mutation detected under registered root"
        );
        assert_eq!(
            self.names.keys().collect::<Vec<_>>(),
            real_files.keys().collect::<Vec<_>>(),
            "raw std::fs name mutation detected under registered root"
        );

        for (path, bytes) in real_files {
            let id = self.names.get(&path).unwrap_or_else(|| {
                panic!(
                    "real file is missing from crashsim model: '{}'",
                    self.root.join(&path).display()
                )
            });
            let modeled = self.files.get(id).unwrap_or_else(|| {
                panic!(
                    "modeled file contents are missing for '{}'",
                    self.root.join(&path).display()
                )
            });
            assert_eq!(
                modeled,
                &bytes,
                "raw std::fs content mutation detected under registered root: '{}'",
                path.display()
            );
        }
    }
}

fn registry() -> &'static Mutex<BTreeMap<PathBuf, std::sync::Weak<Mutex<State>>>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, std::sync::Weak<Mutex<State>>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn absolute_normalized(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        canonical_or_normalized(path)
    } else {
        canonical_or_normalized(&std::env::current_dir()?.join(path))
    }
}

fn canonical_or_normalized(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent = fs::canonicalize(parent)?;
            Ok(parent.join(path.file_name().unwrap_or_default()))
        }
        Err(error) => Err(error),
    }
}

fn ignored_by(ephemeral_data_roots: &BTreeSet<PathBuf>, relative: &Path) -> bool {
    ephemeral_data_roots.iter().any(|root| {
        let Ok(within_root) = relative.strip_prefix(root) else {
            return false;
        };
        let mut components = within_root.components();
        let Some(Component::Normal(name)) = components.next() else {
            return false;
        };

        if name == "spill" || name.to_string_lossy().starts_with(".htap-ipc-") {
            return true;
        }
        if components.next().is_some() {
            return false;
        }

        name == "LOCK" || name == "htap.sock"
    })
}

fn read_all(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path)
}

fn collect_tree(
    absolute: &Path,
    relative: &Path,
    ephemeral_data_roots: &BTreeSet<PathBuf>,
    directories: &mut BTreeSet<PathBuf>,
    files: &mut BTreeMap<PathBuf, Vec<u8>>,
) -> io::Result<()> {
    for entry in fs::read_dir(absolute)? {
        let entry = entry?;
        let child_relative = relative.join(entry.file_name());
        if ignored_by(ephemeral_data_roots, &child_relative) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            directories.insert(child_relative.clone());
            collect_tree(
                &entry.path(),
                &child_relative,
                ephemeral_data_roots,
                directories,
                files,
            )?;
        } else if metadata.is_file() {
            files.insert(child_relative, read_all(&entry.path())?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn records_create_under_symlinked_root() {
        use std::io::Write;
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real_root = temp.path().join("real");
        let linked_root = temp.path().join("linked");
        fs::create_dir(&real_root).unwrap();
        symlink(&real_root, &linked_root).unwrap();

        let recorder = register(&linked_root).unwrap();
        let path = linked_root.join("created");
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();

        let handle = open_file(&file, &path, false, false, true, false)
            .expect("symlinked path should be associated with the recorder");
        file.write_all(b"recorded").unwrap();
        write_file(&handle, b"recorded");

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("created")];
        assert_eq!(snapshot.files[&file_id], b"recorded");
        assert_eq!(fs::read(path).unwrap(), b"recorded");
        recorder.verify_tree();
    }

    #[test]
    #[should_panic(expected = "path unreachable from registered root")]
    fn unresolvable_path_under_root_panics() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let path = recorder.root().join("missing-parent").join("child");
        mkdir(&path);
    }

    #[test]
    fn rename_preserves_file_identity_for_open_handles() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let from = recorder.root().join("from");
        let to = recorder.root().join("to");

        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&from)
            .unwrap();
        let handle = open_file(&file, &from, false, false, true, false).unwrap();

        file.write_all(b"before").unwrap();
        write_file(&handle, b"before");

        fs::rename(&from, &to).unwrap();
        rename(&from, &to);

        file.write_all(b"-after").unwrap();
        write_file(&handle, b"-after");

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("to")];
        assert_eq!(snapshot.files[&file_id], b"before-after");
        assert!(!snapshot.names.contains_key(Path::new("from")));
        recorder.verify_tree();
    }

    #[test]
    fn mkdir_records_new_directory() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let directory = recorder.root().join("created");

        fs::create_dir(&directory).unwrap();
        mkdir(&directory);

        let snapshot = recorder.snapshot();
        assert!(snapshot.directories.contains(Path::new("created")));
        assert!(snapshot
            .log
            .iter()
            .any(|op| matches!(op, Op::Mkdir { path } if path == Path::new("created"))));
        recorder.verify_tree();
    }

    #[test]
    #[should_panic(expected = "unsupported in crash model")]
    fn rename_over_existing_directory_panics() {
        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let from = recorder.root().join("from");
        let to = recorder.root().join("to");

        fs::create_dir(&from).unwrap();
        mkdir(&from);
        fs::create_dir(&to).unwrap();
        mkdir(&to);

        rename(&from, &to);
    }

    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "path escapes registered root")]
    fn symlinked_path_that_escapes_root_panics() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();

        let recorder = register(&root).unwrap();
        let escape = recorder.root().join("escape");
        symlink(&outside, &escape).unwrap();

        let escaped_file = escape.join("data");
        fs::write(&escaped_file, b"outside").unwrap();

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&escaped_file)
            .unwrap();
        open_file(&file, &escaped_file, false, false, false, true);
    }

    #[test]
    fn write_only_open_of_existing_file_models_content() {
        use super::super::dur::DurOpenOptions;
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing");
        fs::write(&path, b"original").unwrap();

        let recorder = register(temp.path()).unwrap();
        let mut file = DurOpenOptions::new().write(true).open(&path).unwrap();

        file.write_all(b"NEW").unwrap();
        file.sync_all().unwrap();

        let snapshot = recorder.snapshot();
        let file_id = snapshot.names[Path::new("existing")];
        assert_eq!(snapshot.files[&file_id], b"NEWginal");
        assert_eq!(fs::read(&path).unwrap(), b"NEWginal");
        recorder.verify_tree();
    }

    #[test]
    fn baseline_captured_at_register() {
        use super::super::dur::DurOpenOptions;
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("existing-dir");
        let existing = directory.join("existing");
        let created = temp.path().join("created");
        fs::create_dir(&directory).unwrap();
        fs::write(&existing, b"original").unwrap();

        let recorder = register(temp.path()).unwrap();

        let mut existing_file = DurOpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&existing)
            .unwrap();
        existing_file.write_all(b"overwritten").unwrap();

        let mut created_file = DurOpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&created)
            .unwrap();
        created_file.write_all(b"new").unwrap();

        let snapshot = recorder.snapshot();
        let existing_path = Path::new("existing-dir").join("existing");
        let baseline_file_id = snapshot.baseline_names[&existing_path];

        assert!(snapshot
            .baseline_directories
            .contains(Path::new("existing-dir")));
        assert_eq!(snapshot.baseline_files[&baseline_file_id], b"original");
        assert!(!snapshot.baseline_names.contains_key(Path::new("created")));

        let current_file_id = snapshot.names[&existing_path];
        let created_file_id = snapshot.names[Path::new("created")];
        assert_eq!(snapshot.files[&current_file_id], b"overwritten");
        assert_eq!(snapshot.files[&created_file_id], b"new");
        recorder.verify_tree();
    }

    #[test]
    fn rename_over_records_replaced_id() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let a = recorder.root().join("a");
        let b = recorder.root().join("b");

        let mut file_a = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&a)
            .unwrap();
        let handle_a = open_file(&file_a, &a, false, false, true, false).unwrap();
        file_a.write_all(b"a").unwrap();
        write_file(&handle_a, b"a");
        file_a.sync_all().unwrap();
        sync_file(&handle_a, true, None);

        let mut file_b = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&b)
            .unwrap();
        let handle_b = open_file(&file_b, &b, false, false, true, false).unwrap();
        file_b.write_all(b"b").unwrap();
        write_file(&handle_b, b"b");
        file_b.sync_all().unwrap();
        sync_file(&handle_b, true, None);

        let b_id = recorder.snapshot().names[Path::new("b")];
        fs::rename(&a, &b).unwrap();
        rename(&a, &b);

        assert!(recorder.snapshot().log.iter().any(|op| {
            matches!(
                op,
                Op::Rename {
                    from,
                    to,
                    file_id: _,
                    replaced: Some(replaced),
                } if from == Path::new("a") && to == Path::new("b") && *replaced == b_id
            )
        }));
        recorder.verify_tree();
    }

    #[test]
    fn write_through_handle_on_replaced_file_does_not_panic() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let a = recorder.root().join("a");
        let b = recorder.root().join("b");

        let mut file_a = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&a)
            .unwrap();
        let handle_a = open_file(&file_a, &a, false, false, true, false).unwrap();
        file_a.write_all(b"a").unwrap();
        write_file(&handle_a, b"a");

        let mut file_b = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&b)
            .unwrap();
        let handle_b = open_file(&file_b, &b, false, false, true, false).unwrap();
        file_b.write_all(b"b").unwrap();
        write_file(&handle_b, b"b");
        let b_id = recorder.snapshot().names[Path::new("b")];

        fs::rename(&a, &b).unwrap();
        rename(&a, &b);

        file_b.write_all(b"-after").unwrap();
        write_file(&handle_b, b"-after");
        file_b.sync_all().unwrap();
        sync_file(&handle_b, true, None);

        assert_eq!(recorder.snapshot().files[&b_id], b"b-after");
        recorder.verify_tree();
    }

    #[test]
    fn fsync_of_unlinked_open_file_does_not_panic() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let recorder = register(temp.path()).unwrap();
        let path = recorder.root().join("file");

        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let handle = open_file(&file, &path, false, false, true, false).unwrap();
        file.write_all(b"contents").unwrap();
        write_file(&handle, b"contents");
        let file_id = recorder.snapshot().names[Path::new("file")];

        fs::remove_file(&path).unwrap();
        remove_file(&path);
        file.sync_all().unwrap();
        sync_file(&handle, true, None);

        assert_eq!(recorder.snapshot().files[&file_id], b"contents");
        recorder.verify_tree();
    }

    #[test]
    #[should_panic(expected = "unsupported in crash model: '..' path component")]
    fn dotdot_path_that_escapes_root_panics() {
        use super::super::dur::DurOpenOptions;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();

        let recorder = register(&root).unwrap();
        let escaped_file = recorder.root().join("..").join("escaped");
        let _file = DurOpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&escaped_file)
            .unwrap();
    }
}
