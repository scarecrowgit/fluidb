use crate::model::{CrashImage, ForcedTree};
use crate::policy::CrashPolicy;
use htap_common::fs::Op;
use std::fs;
use std::io;
use std::path::Path;

pub fn materialize(
    crash_image: &CrashImage,
    policy: &CrashPolicy,
    temp_root: &Path,
) -> io::Result<()> {
    let mut tree = crash_image.build_forced_tree();

    match policy {
        CrashPolicy::Strict => {}
        CrashPolicy::Torn { seed, sector_size } => {
            if *sector_size == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "crashsim sector size must be non-zero",
                ));
            }

            let observed = crash_image.build_observed_tree();
            let mut rng = DeterministicRng::new(*seed);
            merge_dirty_sectors(&mut tree, &observed, *sector_size, &mut rng);
        }
        CrashPolicy::Chaos { seed } => {
            tree = crash_image.build_initial_tree();
            let mut rng = DeterministicRng::new(*seed);

            for (index, op) in crash_image.operations().iter().enumerate() {
                if crash_image.is_forced(index) {
                    crash_image.apply(&mut tree, index);
                    continue;
                }

                let probability = if is_namespace_operation(op) {
                    (3, 4)
                } else {
                    (1, 2)
                };
                if rng.choose(probability.0, probability.1) {
                    crash_image.apply(&mut tree, index);
                }
            }
        }
    }

    tree.retain_reachable();
    write_tree(&tree, temp_root)
}

fn is_namespace_operation(op: &Op) -> bool {
    matches!(
        op,
        Op::Mkdir { .. }
            | Op::Create { .. }
            | Op::Rename { .. }
            | Op::Unlink { .. }
            | Op::Rmdir { .. }
    )
}

fn merge_dirty_sectors(
    tree: &mut ForcedTree,
    observed: &ForcedTree,
    sector_size: usize,
    rng: &mut DeterministicRng,
) {
    let file_ids: Vec<_> = tree.names.values().copied().collect();

    for file_id in file_ids {
        let strict = tree.files.entry(file_id).or_default().clone();
        let observed = observed.files.get(&file_id).cloned().unwrap_or_default();

        let chosen_len = if strict.len() == observed.len() || !rng.choose(1, 2) {
            strict.len()
        } else {
            observed.len()
        };

        let mut merged = strict.clone();
        merged.resize(chosen_len, 0);

        for sector in 0..chosen_len.div_ceil(sector_size) {
            let start = sector * sector_size;
            let end = (start + sector_size).min(chosen_len);
            let strict_bytes = strict.get(start..end);
            let observed_bytes = observed.get(start..end);

            if strict_bytes == observed_bytes || !rng.choose(1, 2) {
                continue;
            }

            for (offset, byte) in merged[start..end].iter_mut().enumerate() {
                if let Some(observed_byte) = observed.get(start + offset) {
                    *byte = *observed_byte;
                }
            }
        }

        tree.files.insert(file_id, merged);
    }
}

fn write_tree(tree: &ForcedTree, temp_root: &Path) -> io::Result<()> {
    for directory in tree.directories() {
        if directory.as_os_str().is_empty() {
            continue;
        }

        let destination = temp_root.join(directory);
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "directory has no parent"))?;

        if !fs::metadata(parent)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "directory parent is not a directory",
            ));
        }

        fs::create_dir(destination)?;
    }

    for (path, content) in tree.files() {
        let destination = temp_root.join(path);
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file has no parent"))?;

        if !fs::metadata(parent)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "file parent is not a directory",
            ));
        }

        fs::write(destination, content)?;
    }

    Ok(())
}

struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn choose(&mut self, numerator: usize, denominator: usize) -> bool {
        debug_assert!(numerator <= denominator);
        denominator != 0 && (self.next() % denominator as u64) < numerator as u64
    }
}
