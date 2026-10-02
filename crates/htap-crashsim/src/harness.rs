use std::collections::BTreeSet;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use htap_common::fs::{register, Op, Recorder, Snapshot};
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::materialize::materialize;
use crate::model::CrashImage;
use crate::policy::{
    parse_recovery_repro_string, parse_repro_string, CrashInfo, CrashPolicy, RecoveryReproPoint,
    RecoveryStage,
};

/// Records a workload and enumerates its possible crash points.
pub struct CrashHarness {
    name: String,
    temp_dir: TempDir,
    root: PathBuf,
    recorder: Recorder,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

impl CrashHarness {
    pub fn new(name: impl Into<String>) -> io::Result<Self> {
        let name = name.into();
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path().join("workload");
        fs::create_dir(&root)?;
        let recorder = register(&root)?;

        Ok(Self {
            name,
            temp_dir,
            root,
            recorder,
            snapshot: Arc::new(Mutex::new(None)),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_workload<F>(&self, workload: F) -> io::Result<()>
    where
        F: FnOnce(&WorkloadContext),
    {
        let context = WorkloadContext {
            root: self.root.clone(),
            recorder: self.recorder.clone(),
        };

        workload(&context);
        self.recorder.verify_tree();
        *self.snapshot.lock() = Some(self.recorder.snapshot());
        Ok(())
    }

    pub fn enumerate<F>(&self, policy: &CrashPolicy, mut checker: F) -> io::Result<()>
    where
        F: FnMut(&Path, &CrashInfo),
    {
        let snapshot = self.snapshot()?;
        self.enumerate_snapshot(snapshot, policy, selection_config_from_env(), &mut checker)
    }

    pub fn enumerate_with_env<'a, F>(
        &self,
        policy: &CrashPolicy,
        env: impl IntoIterator<Item = (&'a str, &'a str)>,
        mut checker: F,
    ) -> io::Result<()>
    where
        F: FnMut(&Path, &CrashInfo),
    {
        let snapshot = self.snapshot()?;
        self.enumerate_snapshot(
            snapshot,
            policy,
            selection_config_from_values(env),
            &mut checker,
        )
    }

    /// Replays the crash image identified by `POWERLOSS_REPRO`, if present.
    pub fn replay_repro<F>(&self, checker: F) -> io::Result<bool>
    where
        F: FnMut(&Path, &CrashInfo),
    {
        let Some(repro) = std::env::var("POWERLOSS_REPRO").ok() else {
            return Ok(false);
        };

        self.replay(&repro, checker)?;
        Ok(true)
    }

    /// Replays one crash image identified by a `POWERLOSS_REPRO` value.
    pub fn replay<F>(&self, repro: &str, mut checker: F) -> io::Result<()>
    where
        F: FnMut(&Path, &CrashInfo),
    {
        let (name, variant, seed, crash_point) = parse_repro_string(repro)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid repro string"))?;
        if name != self.name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repro string belongs to another harness",
            ));
        }

        let policy = policy_from_repro_variant(&variant, seed)?;

        let snapshot = self.snapshot()?;
        if crash_point > snapshot.log.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repro crash point exceeds workload log",
            ));
        }

        let root = self.temp_dir.path().join(format!(
            "{}-replay-{}-{seed}-k={crash_point}",
            sanitize_name(&self.name),
            policy.short_name()
        ));
        recreate_dir(&root)?;
        let image = CrashImage::compute(snapshot.clone(), crash_point);
        materialize(&image, &policy, &root)?;

        let info = CrashInfo {
            harness_name: &self.name,
            crash_point,
            policy,
            ops: &snapshot.log[..crash_point],
            acked_labels: acked_labels(&snapshot.log, crash_point),
            recovery: None,
        };
        run_checker(&self.name, &root, &info, &mut checker);
        Ok(())
    }

    /// Replays the recovery image identified by `POWERLOSS_REPRO`, if present.
    pub fn replay_recovery_repro<R, F>(&self, recover: R, check: F) -> io::Result<bool>
    where
        R: FnMut(&Path),
        F: FnMut(&Path, &CrashInfo),
    {
        let Some(repro) = std::env::var("POWERLOSS_REPRO").ok() else {
            return Ok(false);
        };

        self.replay_recovery(&repro, recover, check)?;
        Ok(true)
    }

    /// Replays one recovery image identified by a two-stage `POWERLOSS_REPRO` value.
    pub fn replay_recovery<R, F>(&self, repro: &str, mut recover: R, mut check: F) -> io::Result<()>
    where
        R: FnMut(&Path),
        F: FnMut(&Path, &CrashInfo),
    {
        let (name, variant, seed, outer_point, recovery_point) = parse_recovery_repro_string(repro)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid recovery repro string")
            })?;
        if name != self.name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repro string belongs to another harness",
            ));
        }

        let policy = policy_from_repro_variant(&variant, seed)?;
        let snapshot = self.snapshot()?;
        if outer_point > snapshot.log.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repro crash point exceeds workload log",
            ));
        }

        let outer_root = self.temp_dir.path().join(format!(
            "{}-recovery-replay-outer-{}-{seed}-k={outer_point}",
            sanitize_name(&self.name),
            policy.short_name()
        ));
        recreate_dir(&outer_root)?;
        materialize(
            &CrashImage::compute(snapshot.clone(), outer_point),
            &policy,
            &outer_root,
        )?;

        let recovery_root = self.temp_dir.path().join(format!(
            "{}-recovery-replay-{}-{seed}-k={outer_point}",
            sanitize_name(&self.name),
            policy.short_name()
        ));
        recreate_dir(&recovery_root)?;
        copy_tree(&outer_root, &recovery_root)?;

        let outer_info = CrashInfo {
            harness_name: &self.name,
            crash_point: outer_point,
            policy: policy.clone(),
            ops: &snapshot.log[..outer_point],
            acked_labels: acked_labels(&snapshot.log, outer_point),
            recovery: None,
        };
        let recorder = register(&recovery_root)?;
        run_recovery(
            &self.name,
            &recovery_root,
            &outer_info,
            &mut recover,
            &recorder,
        );
        let recovery = recorder.snapshot();
        drop(recorder);

        if recovery_point == RecoveryReproPoint::Recover {
            return Err(io::Error::other(
                "recovery did not panic on replay of rk=recover",
            ));
        }

        let (root, recovery_stage) = match recovery_point {
            RecoveryReproPoint::Crash(recovery_point) => {
                if recovery_point > recovery.log.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "repro recovery crash point exceeds recovery log",
                    ));
                }

                let image_root = self.temp_dir.path().join(format!(
                    "{}-recovery-replay-image-{seed}-{outer_point}-k={recovery_point}",
                    sanitize_name(&self.name)
                ));
                recreate_dir(&image_root)?;
                materialize(
                    &CrashImage::compute(recovery.clone(), recovery_point),
                    &CrashPolicy::Strict,
                    &image_root,
                )?;
                (
                    image_root,
                    RecoveryStage {
                        crash_point: recovery_point,
                        completed: false,
                        ops: &recovery.log[..recovery_point],
                        acked_labels: acked_labels(&recovery.log, recovery_point),
                    },
                )
            }
            RecoveryReproPoint::Done => (
                recovery_root,
                RecoveryStage {
                    crash_point: recovery.log.len(),
                    completed: true,
                    ops: &recovery.log,
                    acked_labels: acked_labels(&recovery.log, recovery.log.len()),
                },
            ),
            RecoveryReproPoint::Recover => unreachable!("handled above"),
        };

        let info = CrashInfo {
            harness_name: &self.name,
            crash_point: outer_point,
            policy,
            ops: &snapshot.log[..outer_point],
            acked_labels: acked_labels(&snapshot.log, outer_point),
            recovery: Some(recovery_stage),
        };
        run_checker(&self.name, &root, &info, &mut check);
        Ok(())
    }

    /// Enumerates one additional crash during recovery of each selected image.
    pub fn enumerate_recovery<R, F>(
        &self,
        policy: &CrashPolicy,
        mut recover: R,
        mut check: F,
    ) -> io::Result<()>
    where
        R: FnMut(&Path),
        F: FnMut(&Path, &CrashInfo),
    {
        let snapshot = self.snapshot()?;
        let config = selection_config_from_env();
        let mut outer_images = BTreeSet::new();

        for crash_point in select_crash_points(&snapshot.log, &config) {
            for selected_policy in policy_instances(policy, config.seed_count) {
                let outer_root = self.temp_dir.path().join(format!(
                    "{}-recovery-outer-{}-{}-k={crash_point}",
                    sanitize_name(&self.name),
                    selected_policy.short_name(),
                    policy_seed(&selected_policy)
                ));
                recreate_dir(&outer_root)?;
                materialize(
                    &CrashImage::compute(snapshot.clone(), crash_point),
                    &selected_policy,
                    &outer_root,
                )?;

                let outer_acked_labels = acked_label_set(&snapshot.log, crash_point);
                let outer_key = (tree_hash(&outer_root)?, outer_acked_labels.clone());
                if !config.exhaustive && !outer_images.insert(outer_key) {
                    continue;
                }

                let outer_info = CrashInfo {
                    harness_name: &self.name,
                    crash_point,
                    policy: selected_policy.clone(),
                    ops: &snapshot.log[..crash_point],
                    acked_labels: acked_labels(&snapshot.log, crash_point),
                    recovery: None,
                };
                let recovery_root = self.temp_dir.path().join(format!(
                    "{}-recovery-{}-{}-k={crash_point}",
                    sanitize_name(&self.name),
                    selected_policy.short_name(),
                    policy_seed(&selected_policy)
                ));
                recreate_dir(&recovery_root)?;
                copy_tree(&outer_root, &recovery_root)?;

                let recorder = register(&recovery_root)?;
                run_recovery(
                    &self.name,
                    &recovery_root,
                    &outer_info,
                    &mut recover,
                    &recorder,
                );
                let recovery = recorder.snapshot();
                drop(recorder);

                let mut recovery_images = BTreeSet::new();
                let strict = CrashPolicy::Strict;
                for recovery_point in select_crash_points(&recovery.log, &config) {
                    let image_root = self.temp_dir.path().join(format!(
                        "{}-recovery-image-{}-{crash_point}-k={recovery_point}",
                        sanitize_name(&self.name),
                        policy_seed(&selected_policy)
                    ));
                    recreate_dir(&image_root)?;
                    materialize(
                        &CrashImage::compute(recovery.clone(), recovery_point),
                        &strict,
                        &image_root,
                    )?;

                    let recovery_key = (tree_hash(&image_root)?, outer_acked_labels.clone());
                    if config.exhaustive || recovery_images.insert(recovery_key) {
                        let info = CrashInfo {
                            harness_name: &self.name,
                            crash_point,
                            policy: selected_policy.clone(),
                            ops: &snapshot.log[..crash_point],
                            acked_labels: acked_labels(&snapshot.log, crash_point),
                            recovery: Some(RecoveryStage {
                                crash_point: recovery_point,
                                completed: false,
                                ops: &recovery.log[..recovery_point],
                                acked_labels: acked_labels(&recovery.log, recovery_point),
                            }),
                        };
                        run_checker(&self.name, &image_root, &info, &mut check);
                    }
                }

                // Always expose the successful recovery result, including an empty log.
                let completed_info = CrashInfo {
                    harness_name: &self.name,
                    crash_point,
                    policy: selected_policy,
                    ops: &snapshot.log[..crash_point],
                    acked_labels: acked_labels(&snapshot.log, crash_point),
                    recovery: Some(RecoveryStage {
                        crash_point: recovery.log.len(),
                        completed: true,
                        ops: &recovery.log,
                        acked_labels: acked_labels(&recovery.log, recovery.log.len()),
                    }),
                };
                run_checker(&self.name, &recovery_root, &completed_info, &mut check);
            }
        }

        Ok(())
    }

    pub fn snapshot(&self) -> io::Result<Snapshot> {
        self.snapshot
            .lock()
            .clone()
            .ok_or_else(|| io::Error::other("workload has not been run"))
    }

    #[doc(hidden)]
    pub fn default_crash_points(&self) -> io::Result<Vec<usize>> {
        let snapshot = self.snapshot()?;
        Ok(select_crash_points(
            &snapshot.log,
            &SelectionConfig {
                exhaustive: false,
                seed_count: 2,
            },
        ))
    }

    fn enumerate_snapshot<F>(
        &self,
        snapshot: Snapshot,
        policy: &CrashPolicy,
        config: SelectionConfig,
        checker: &mut F,
    ) -> io::Result<()>
    where
        F: FnMut(&Path, &CrashInfo),
    {
        let mut seen = BTreeSet::new();

        for crash_point in select_crash_points(&snapshot.log, &config) {
            for selected_policy in policy_instances(policy, config.seed_count) {
                let seed = policy_seed(&selected_policy);
                let crash_root = self.temp_dir.path().join(format!(
                    "{}-{}-{seed}-k={crash_point}",
                    sanitize_name(&self.name),
                    selected_policy.short_name()
                ));
                recreate_dir(&crash_root)?;

                let crash_image = CrashImage::compute(snapshot.clone(), crash_point);
                materialize(&crash_image, &selected_policy, &crash_root)?;
                let key = (
                    tree_hash(&crash_root)?,
                    acked_label_set(&snapshot.log, crash_point),
                );
                if !config.exhaustive && !seen.insert(key) {
                    continue;
                }

                let info = CrashInfo {
                    harness_name: &self.name,
                    crash_point,
                    policy: selected_policy,
                    ops: &snapshot.log[..crash_point],
                    acked_labels: acked_labels(&snapshot.log, crash_point),
                    recovery: None,
                };
                run_checker(&self.name, &crash_root, &info, checker);
            }
        }

        Ok(())
    }
}

/// Context supplied to a recorded workload.
pub struct WorkloadContext {
    root: PathBuf,
    recorder: Recorder,
}

impl WorkloadContext {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ack(&self, label: impl Into<String>) {
        self.recorder.ack(label);
    }
}

fn acked_labels(log: &[Op], crash_point: usize) -> Vec<String> {
    log[..crash_point]
        .iter()
        .filter_map(|op| match op {
            Op::Ack { label } => Some(label.clone()),
            _ => None,
        })
        .collect()
}

fn acked_label_set(log: &[Op], crash_point: usize) -> BTreeSet<String> {
    acked_labels(log, crash_point).into_iter().collect()
}

#[derive(Clone, Copy)]
struct SelectionConfig {
    exhaustive: bool,
    seed_count: usize,
}

fn selection_config_from_env() -> SelectionConfig {
    let exhaustive = std::env::var("POWERLOSS_EXHAUSTIVE").ok().as_deref() == Some("1");
    let seed_count = std::env::var("POWERLOSS_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    assert!(seed_count >= 1, "POWERLOSS_SEEDS must be at least 1");

    SelectionConfig {
        exhaustive,
        seed_count,
    }
}

fn selection_config_from_values<'a>(
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> SelectionConfig {
    let mut config = SelectionConfig {
        exhaustive: false,
        seed_count: 2,
    };

    for (key, value) in env {
        match key {
            "POWERLOSS_EXHAUSTIVE" => config.exhaustive = value == "1",
            "POWERLOSS_SEEDS" => {
                if let Ok(seed_count) = value.parse() {
                    config.seed_count = seed_count;
                }
            }
            _ => {}
        }
    }

    assert!(config.seed_count >= 1, "POWERLOSS_SEEDS must be at least 1");
    config
}

fn select_crash_points(log: &[Op], config: &SelectionConfig) -> Vec<usize> {
    if config.exhaustive {
        return (0..=log.len()).collect();
    }

    let mut selected = BTreeSet::from([0, log.len()]);
    let mut writes = Vec::new();

    for (index, op) in log.iter().enumerate() {
        match op {
            Op::FsyncFile { .. } | Op::FsyncDir { .. } => {
                // Keep both the dirty state before the sync and the durable state after it.
                selected.insert(index);
                selected.insert(index + 1);
            }
            Op::Create { .. }
            | Op::Mkdir { .. }
            | Op::Rename { .. }
            | Op::Unlink { .. }
            | Op::Rmdir { .. } => {
                selected.insert(index + 1);
            }
            Op::Write { .. } | Op::SetLen { .. } => writes.push(index + 1),
            Op::Ack { .. } => {
                selected.insert(index + 1);
            }
        }
    }

    // Deterministically sample up to eight write boundaries without privileging
    // early writes in a long workload.
    let mut rng = SelectionRng::new(log.len() as u64);
    while writes.len() > 8 {
        writes.remove((rng.next() as usize) % writes.len());
    }
    selected.extend(writes);
    selected.into_iter().collect()
}

fn policy_from_repro_variant(variant: &str, seed: u64) -> io::Result<CrashPolicy> {
    match variant {
        "strict" => Ok(CrashPolicy::Strict),
        "torn" => Ok(CrashPolicy::Torn {
            seed,
            sector_size: 4096,
        }),
        "chaos" => Ok(CrashPolicy::Chaos { seed }),
        _ => variant
            .strip_prefix("torn-s=")
            .and_then(|value| value.parse().ok())
            .map(|sector_size| CrashPolicy::Torn { seed, sector_size })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown repro policy")),
    }
}

fn policy_instances(policy: &CrashPolicy, seed_count: usize) -> Vec<CrashPolicy> {
    match policy {
        CrashPolicy::Strict => vec![CrashPolicy::Strict],
        CrashPolicy::Torn { seed, sector_size } => (0..seed_count)
            .map(|offset| CrashPolicy::Torn {
                seed: seed.wrapping_add(offset as u64),
                sector_size: *sector_size,
            })
            .collect(),
        CrashPolicy::Chaos { seed } => vec![CrashPolicy::Chaos { seed: *seed }],
    }
}

fn policy_seed(policy: &CrashPolicy) -> u64 {
    match policy {
        CrashPolicy::Strict => 0,
        CrashPolicy::Torn { seed, .. } | CrashPolicy::Chaos { seed } => *seed,
    }
}

fn run_checker<F>(_name: &str, root: &Path, info: &CrashInfo, checker: &mut F)
where
    F: FnMut(&Path, &CrashInfo),
{
    if panic::catch_unwind(AssertUnwindSafe(|| checker(root, info))).is_err() {
        let repro = info.repro_string();
        eprintln!("POWERLOSS_REPRO={repro}");
        panic!("POWERLOSS_REPRO={repro}");
    }
}

fn run_recovery<R>(_name: &str, root: &Path, info: &CrashInfo, recover: &mut R, recorder: &Recorder)
where
    R: FnMut(&Path),
{
    if panic::catch_unwind(AssertUnwindSafe(|| {
        recover(root);
        recorder.verify_tree();
    }))
    .is_err()
    {
        let repro = format!("{}/rk=recover", info.repro_string());
        eprintln!("POWERLOSS_REPRO={repro}");
        panic!("POWERLOSS_REPRO={repro}");
    }
}

fn recreate_dir(path: &Path) -> io::Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir(path)
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn tree_hash(root: &Path) -> io::Result<u64> {
    fn hash_tree(path: &Path, relative: &Path, hasher: &mut impl Hasher) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let name = entry.file_name();
            let child_relative = relative.join(&name);
            child_relative.hash(hasher);
            if entry.file_type()?.is_dir() {
                0_u8.hash(hasher);
                hash_tree(&entry.path(), &child_relative, hasher)?;
            } else {
                1_u8.hash(hasher);
                fs::read(entry.path())?.hash(hasher);
            }
        }
        Ok(())
    }

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_tree(root, Path::new(""), &mut hasher)?;
    Ok(hasher.finish())
}

struct SelectionRng {
    state: u64,
}

impl SelectionRng {
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
}

fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "crashsim".to_owned()
    } else {
        sanitized
    }
}
