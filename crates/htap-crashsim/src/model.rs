use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use htap_common::fs::{FileId, Op, Snapshot};

/// The subset of operations that must be reflected after a crash at `k`.
pub struct CrashImage {
    initial: Snapshot,
    forced_log_indices: BTreeSet<usize>,
    parent_generations: BTreeMap<usize, u64>,
    directory_generations: BTreeMap<usize, u64>,
    crash_point: usize,
}

impl CrashImage {
    /// Computes the operations forced by syncs before crash point `k`.
    pub fn compute(initial: Snapshot, k: usize) -> Self {
        assert!(
            k <= initial.log.len(),
            "crash point exceeds recorded operation log"
        );

        let (parent_generations, directory_generations) = tag_generations(&initial);
        let mut forced_log_indices = BTreeSet::new();

        for (index, op) in initial.log[..k].iter().enumerate() {
            match op {
                Op::Write { file_id, .. } | Op::SetLen { file_id, .. } => {
                    if initial.log[index + 1..k].iter().any(
                        |later| matches!(later, Op::FsyncFile { file_id: synced, .. } if synced == file_id),
                    ) {
                        forced_log_indices.insert(index);
                    }
                }
                Op::Create { path, .. }
                | Op::Mkdir { path }
                | Op::Unlink { path, .. }
                | Op::Rmdir { path }
                | Op::Rename { from: path, .. } => {
                    let parent_generation = parent_generations[&index];
                    if directory_synced(
                        &initial,
                        &directory_generations,
                        &parent(path),
                        parent_generation,
                        index + 1,
                        k,
                    ) {
                        forced_log_indices.insert(index);
                    }
                }
                Op::FsyncFile { .. } | Op::FsyncDir { .. } | Op::Ack { .. } => {}
            }
        }

        Self {
            initial,
            forced_log_indices,
            parent_generations,
            directory_generations,
            crash_point: k,
        }
    }

    /// Builds the filesystem tree before any recorded operations are applied.
    pub(crate) fn build_initial_tree(&self) -> ForcedTree {
        ForcedTree::initial_from_snapshot(&self.initial)
    }

    /// Builds the filesystem tree containing only crash-forced changes.
    pub fn build_forced_tree(&self) -> ForcedTree {
        ForcedTree::from_crash_image(self)
    }

    pub(crate) fn build_observed_tree(&self) -> ForcedTree {
        let mut tree = ForcedTree::initial_from_snapshot(&self.initial);
        for index in 0..self.crash_point {
            self.apply(&mut tree, index);
        }
        tree.retain_reachable();
        tree
    }

    pub(crate) fn operations(&self) -> &[Op] {
        &self.initial.log[..self.crash_point]
    }

    pub(crate) fn is_forced(&self, index: usize) -> bool {
        self.forced_log_indices.contains(&index)
    }

    pub(crate) fn apply(&self, tree: &mut ForcedTree, index: usize) {
        tree.apply(
            &self.initial.log[index],
            self.parent_generations.get(&index).copied(),
            self.directory_generations.get(&index).copied(),
        );
    }
}

fn tag_generations(snapshot: &Snapshot) -> (BTreeMap<usize, u64>, BTreeMap<usize, u64>) {
    let mut current_generations: BTreeMap<PathBuf, u64> = snapshot
        .baseline_directories
        .iter()
        .cloned()
        .map(|path| (path, 0))
        .collect();
    current_generations.insert(PathBuf::new(), 0);

    let mut parent_generations = BTreeMap::new();
    let mut directory_generations = BTreeMap::new();
    let mut next_generation = 1;

    for (index, op) in snapshot.log.iter().enumerate() {
        match op {
            Op::Create { path, .. } | Op::Unlink { path, .. } => {
                parent_generations.insert(index, current_generations[&parent(path)]);
            }
            Op::Mkdir { path } => {
                parent_generations.insert(index, current_generations[&parent(path)]);
                let generation = next_generation;
                next_generation += 1;
                directory_generations.insert(index, generation);
                current_generations.insert(path.clone(), generation);
            }
            Op::Rmdir { path } => {
                parent_generations.insert(index, current_generations[&parent(path)]);
                directory_generations.insert(index, current_generations[path]);
                current_generations.remove(path);
            }
            Op::Rename { from, .. } => {
                parent_generations.insert(index, current_generations[&parent(from)]);
            }
            Op::FsyncDir { path, .. } => {
                directory_generations.insert(index, current_generations[path]);
            }
            Op::Write { .. } | Op::SetLen { .. } | Op::FsyncFile { .. } | Op::Ack { .. } => {}
        }
    }

    (parent_generations, directory_generations)
}

/// A filesystem tree reconstructed from the operations forced at a crash point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForcedTree {
    pub(crate) directories: BTreeMap<PathBuf, Directory>,
    pub(crate) names: BTreeMap<PathBuf, FileId>,
    name_parent_generations: BTreeMap<PathBuf, u64>,
    pub(crate) files: BTreeMap<FileId, Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Directory {
    generation: u64,
    parent_generation: u64,
}

impl ForcedTree {
    fn initial_from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            directories: snapshot
                .baseline_directories
                .iter()
                .cloned()
                .map(|path| {
                    (
                        path,
                        Directory {
                            generation: 0,
                            parent_generation: 0,
                        },
                    )
                })
                .collect(),
            names: snapshot.baseline_names.clone(),
            name_parent_generations: snapshot
                .baseline_names
                .keys()
                .cloned()
                .map(|path| (path, 0))
                .collect(),
            files: snapshot.baseline_files.clone(),
        }
    }

    /// Replays the crash image's forced operations over its initial tree.
    pub fn from_crash_image(image: &CrashImage) -> Self {
        let mut tree = Self::initial_from_snapshot(&image.initial);

        for index in &image.forced_log_indices {
            image.apply(&mut tree, *index);
        }

        tree.retain_reachable();
        tree
    }

    pub(crate) fn apply(
        &mut self,
        op: &Op,
        parent_generation: Option<u64>,
        directory_generation: Option<u64>,
    ) {
        match op {
            Op::Mkdir { path } => {
                let Some(parent_generation) = parent_generation else {
                    return;
                };
                let Some(generation) = directory_generation else {
                    return;
                };
                if self.directory_has_generation(&parent(path), parent_generation)
                    && !self.names.contains_key(path)
                {
                    self.directories.insert(
                        path.clone(),
                        Directory {
                            generation,
                            parent_generation,
                        },
                    );
                }
            }
            Op::Create { path, file_id } => {
                let Some(parent_generation) = parent_generation else {
                    return;
                };
                if self.directory_has_generation(&parent(path), parent_generation)
                    && !self.directories.contains_key(path)
                {
                    self.names.insert(path.clone(), *file_id);
                    self.name_parent_generations
                        .insert(path.clone(), parent_generation);
                    self.files.entry(*file_id).or_default();
                }
            }
            Op::Write {
                file_id,
                offset,
                bytes_written,
            } => {
                let Ok(offset) = usize::try_from(*offset) else {
                    return;
                };
                let Some(end) = offset.checked_add(bytes_written.len()) else {
                    return;
                };
                let file = self.files.entry(*file_id).or_default();
                if file.len() < offset {
                    file.resize(offset, 0);
                }
                if file.len() < end {
                    file.resize(end, 0);
                }
                file[offset..end].copy_from_slice(bytes_written);
            }
            Op::SetLen { file_id, size } => {
                let Ok(size) = usize::try_from(*size) else {
                    return;
                };
                self.files.entry(*file_id).or_default().resize(size, 0);
            }
            Op::Rename {
                from,
                to,
                file_id,
                replaced,
            } => {
                let Some(parent_generation) = parent_generation else {
                    return;
                };
                if !self.directory_has_generation(&parent(to), parent_generation)
                    || self.names.get(from) != Some(file_id)
                    || self.directories.contains_key(to)
                {
                    return;
                }

                if let Some(replaced) = replaced {
                    if self.names.get(to) == Some(replaced) {
                        self.names.remove(to);
                        self.name_parent_generations.remove(to);
                    }
                }

                self.names.remove(from);
                self.name_parent_generations.remove(from);
                self.names.insert(to.clone(), *file_id);
                self.name_parent_generations
                    .insert(to.clone(), parent_generation);
            }
            Op::Unlink { path, file_id } => {
                let Some(parent_generation) = parent_generation else {
                    return;
                };
                if self.directory_has_generation(&parent(path), parent_generation)
                    && self.names.get(path) == Some(file_id)
                {
                    self.names.remove(path);
                    self.name_parent_generations.remove(path);
                }
            }
            Op::Rmdir { path } => {
                let Some(generation) = directory_generation else {
                    return;
                };
                if self
                    .directories
                    .get(path)
                    .is_some_and(|directory| directory.generation == generation)
                {
                    self.directories.remove(path);
                }
            }
            Op::Ack { .. } | Op::FsyncFile { .. } | Op::FsyncDir { .. } => {}
        }
    }

    fn directory_has_generation(&self, path: &Path, generation: u64) -> bool {
        self.directories
            .get(path)
            .is_some_and(|directory| directory.generation == generation)
    }

    pub(crate) fn retain_reachable(&mut self) {
        self.directories.entry(PathBuf::new()).or_insert(Directory {
            generation: 0,
            parent_generation: 0,
        });

        loop {
            let unreachable: Vec<_> =
                self.directories
                    .iter()
                    .filter(|(path, directory)| {
                        !path.as_os_str().is_empty()
                            && !self.directories.get(&parent(path)).is_some_and(|parent| {
                                parent.generation == directory.parent_generation
                            })
                    })
                    .map(|(path, _)| path.clone())
                    .collect();

            if unreachable.is_empty() {
                break;
            }

            for path in unreachable {
                self.directories.remove(&path);
            }
        }

        let unreachable_names: Vec<_> = self
            .names
            .keys()
            .filter(|path| {
                !self
                    .name_parent_generations
                    .get(*path)
                    .is_some_and(|generation| {
                        self.directory_has_generation(&parent(path), *generation)
                    })
            })
            .cloned()
            .collect();

        for path in unreachable_names {
            self.names.remove(&path);
            self.name_parent_generations.remove(&path);
        }

        self.name_parent_generations
            .retain(|path, _| self.names.contains_key(path));
    }

    /// Returns the directories present in the forced tree.
    pub fn directories(&self) -> impl Iterator<Item = &PathBuf> {
        self.directories.keys()
    }

    /// Returns each file name paired with its contents.
    pub fn files(&self) -> impl Iterator<Item = (&PathBuf, &Vec<u8>)> {
        self.names
            .iter()
            .map(|(path, file_id)| (path, &self.files[file_id]))
    }
}

fn directory_synced(
    snapshot: &Snapshot,
    directory_generations: &BTreeMap<usize, u64>,
    directory: &Path,
    generation: u64,
    start: usize,
    end: usize,
) -> bool {
    let directory = snapshot_relative_path(snapshot, directory);

    snapshot.log[start..end]
        .iter()
        .enumerate()
        .any(|(offset, op)| {
            let index = start + offset;
            matches!(
                op,
                Op::FsyncDir { path, .. }
                    if snapshot_relative_path(snapshot, path) == directory
                        && directory_generations.get(&index) == Some(&generation)
            )
        })
}

fn snapshot_relative_path(snapshot: &Snapshot, path: &Path) -> PathBuf {
    if path == snapshot.root {
        return PathBuf::new();
    }

    path.strip_prefix(&snapshot.root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn parent(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}
