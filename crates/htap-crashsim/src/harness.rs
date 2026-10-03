use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use htap_common::fs::{
    current_skip_sync, parse_skip_sync_env, register, scope_skip_hits, Op, Recorder, SkipSync,
    Snapshot,
};
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::materialize::materialize;
use crate::model::CrashImage;
use crate::policy::{
    parse_recovery_repro_string, parse_repro_string, CrashInfo, CrashPolicy, RecoveryReproPoint,
    RecoveryStage,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PowerLossFailurePhase {
    Checker,
    Recovery,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PowerLossFailure {
    pub repro: String,
    pub phase: PowerLossFailurePhase,
    pub message: String,
}

thread_local! {
    static POWERLOSS_FAILURE: RefCell<Option<PowerLossFailure>> = const { RefCell::new(None) };
}

pub fn clear_powerloss_failure() {
    POWERLOSS_FAILURE.with(|failure| *failure.borrow_mut() = None);
}

pub fn set_powerloss_failure(failure: PowerLossFailure) {
    POWERLOSS_FAILURE.with(|current| *current.borrow_mut() = Some(failure));
}

pub fn powerloss_failure() -> Option<PowerLossFailure> {
    POWERLOSS_FAILURE.with(|failure| failure.borrow().clone())
}

/// Records a workload and enumerates its possible crash points.
pub struct CrashHarness {
    name: String,
    temp_dir: TempDir,
    root: PathBuf,
    recorder: Recorder,
    skip: SkipSync,
    ephemeral_data_roots: Vec<PathBuf>,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

impl CrashHarness {
    pub fn new(name: impl Into<String>) -> io::Result<Self> {
        let name = name.into();
        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir.path().join("workload");
        fs::create_dir(&root)?;
        let recorder = register(&root)?;
        let skip = parse_skip_sync_env();
        recorder.set_skip_sync(skip.clone());

        Ok(Self {
            name,
            temp_dir,
            root,
            recorder,
            skip,
            ephemeral_data_roots: Vec::new(),
            snapshot: Arc::new(Mutex::new(None)),
        })
    }

    pub fn with_data_root(mut self, rel: impl AsRef<Path>) -> Self {
        let rel = rel.as_ref().to_path_buf();
        self.recorder.add_ephemeral_data_root(&rel);
        if !self.ephemeral_data_roots.contains(&rel) {
            self.ephemeral_data_roots.push(rel);
        }
        self
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
        let config = selection_config_from_env(&self.skip);
        if let Ok(repro) = std::env::var("POWERLOSS_REPRO") {
            match repro_fits_call(&repro, &self.name, policy, false) {
                Ok(false) => {}
                Ok(true) => match self.replay(&repro, &mut checker) {
                    Ok(()) => panic!("POWERLOSS_REPLAY_NOT_REPRODUCED={repro}"),
                    Err(error) => panic!(
                        "POWERLOSS_REPLAY_FAILED={repro}: workload drift or invalid repro: {error}"
                    ),
                },
                Err(error) => panic!(
                    "POWERLOSS_REPLAY_FAILED={repro}: workload drift or invalid repro: {error}"
                ),
            }
        }

        let snapshot = self.snapshot()?;
        self.enumerate_snapshot(snapshot, policy, config, &mut checker)
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
        let env: Vec<_> = env.into_iter().collect();
        let config = selection_config_from_values(env.iter().copied(), &self.skip);
        let repro = env
            .iter()
            .find_map(|(key, value)| (*key == "POWERLOSS_REPRO").then_some((*value).to_owned()))
            .or_else(|| std::env::var("POWERLOSS_REPRO").ok());

        if let Some(repro) = repro {
            match repro_fits_call(&repro, &self.name, policy, false) {
                Ok(false) => {}
                Ok(true) => match self.replay(&repro, &mut checker) {
                    Ok(()) => panic!("POWERLOSS_REPLAY_NOT_REPRODUCED={repro}"),
                    Err(error) => panic!(
                        "POWERLOSS_REPLAY_FAILED={repro}: workload drift or invalid repro: {error}"
                    ),
                },
                Err(error) => panic!(
                    "POWERLOSS_REPLAY_FAILED={repro}: workload drift or invalid repro: {error}"
                ),
            }
        }

        let snapshot = self.snapshot()?;
        self.enumerate_snapshot(snapshot, policy, config, &mut checker)
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
        run_checker(&self.name, &root, &info, &mut checker, &self.skip);
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
        recorder.set_skip_sync(self.skip.clone());
        for root in &self.ephemeral_data_roots {
            recorder.add_ephemeral_data_root(root);
        }
        run_recovery(
            &self.name,
            &recovery_root,
            &outer_info,
            &mut recover,
            &recorder,
            &self.skip,
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
        run_checker(&self.name, &root, &info, &mut check, &self.skip);
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
        let config = selection_config_from_env(&self.skip);
        if let Ok(repro) = std::env::var("POWERLOSS_REPRO") {
            match repro_fits_call(&repro, &self.name, policy, true) {
                Ok(false) => {}
                Ok(true) => match self.replay_recovery(&repro, &mut recover, &mut check) {
                    Ok(()) => panic!("POWERLOSS_REPLAY_NOT_REPRODUCED={repro}"),
                    Err(error) => panic!(
                        "POWERLOSS_REPLAY_FAILED={repro}: workload drift or invalid repro: {error}"
                    ),
                },
                Err(error) => panic!(
                    "POWERLOSS_REPLAY_FAILED={repro}: workload drift or invalid repro: {error}"
                ),
            }
        }

        let snapshot = self.snapshot()?;
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
                recorder.set_skip_sync(self.skip.clone());
                for root in &self.ephemeral_data_roots {
                    recorder.add_ephemeral_data_root(root);
                }
                run_recovery(
                    &self.name,
                    &recovery_root,
                    &outer_info,
                    &mut recover,
                    &recorder,
                    &self.skip,
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
                        run_checker(&self.name, &image_root, &info, &mut check, &self.skip);
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
                run_checker(
                    &self.name,
                    &recovery_root,
                    &completed_info,
                    &mut check,
                    &self.skip,
                );
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
                run_checker(&self.name, &crash_root, &info, checker, &self.skip);
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

fn selection_config_from_env(harness_skip: &SkipSync) -> SelectionConfig {
    let exhaustive = !matches!(effective_skip_sync(harness_skip), SkipSync::None)
        || std::env::var("POWERLOSS_EXHAUSTIVE").ok().as_deref() == Some("1");
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
    harness_skip: &SkipSync,
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
    if !matches!(effective_skip_sync(harness_skip), SkipSync::None) {
        config.exhaustive = true;
    }
    config
}

fn effective_skip_sync(harness_skip: &SkipSync) -> SkipSync {
    match current_skip_sync() {
        SkipSync::None => harness_skip.clone(),
        scoped => scoped,
    }
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

fn repro_fits_call(
    repro: &str,
    harness_name: &str,
    policy: &CrashPolicy,
    expects_recovery: bool,
) -> io::Result<bool> {
    let (name, variant, seed, is_recovery) =
        if let Some((name, variant, seed, _)) = parse_repro_string(repro) {
            (name, variant, seed, false)
        } else if let Some((name, variant, seed, _, _)) = parse_recovery_repro_string(repro) {
            (name, variant, seed, true)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid repro string",
            ));
        };

    if name != harness_name {
        return Ok(false);
    }

    let repro_policy = policy_from_repro_variant(&variant, seed)?;
    Ok(is_recovery == expects_recovery && policies_equal(&repro_policy, policy))
}

fn policies_equal(left: &CrashPolicy, right: &CrashPolicy) -> bool {
    match (left, right) {
        (CrashPolicy::Strict, CrashPolicy::Strict) => true,
        (
            CrashPolicy::Torn {
                sector_size: left_sector_size,
                ..
            },
            CrashPolicy::Torn {
                sector_size: right_sector_size,
                ..
            },
        ) => left_sector_size == right_sector_size,
        (CrashPolicy::Chaos { .. }, CrashPolicy::Chaos { .. }) => true,
        _ => false,
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

fn run_checker<F>(
    _name: &str,
    root: &Path,
    info: &CrashInfo,
    checker: &mut F,
    harness_skip: &SkipSync,
) where
    F: FnMut(&Path, &CrashInfo),
{
    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| checker(root, info))) {
        let repro = info.repro_string();
        record_powerloss_failure(
            repro.clone(),
            PowerLossFailurePhase::Checker,
            panic_message(payload.as_ref()),
        );
        print_active_skip_sync(harness_skip);
        eprintln!("POWERLOSS_LOG_FINGERPRINT={:016x}", log_fingerprint(info));
        panic!("POWERLOSS_REPRO={repro}");
    }
}

fn run_recovery<R>(
    _name: &str,
    root: &Path,
    info: &CrashInfo,
    recover: &mut R,
    recorder: &Recorder,
    harness_skip: &SkipSync,
) where
    R: FnMut(&Path),
{
    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| {
        recover(root);
        recorder.verify_tree();
    })) {
        let repro = format!("{}/rk=recover", info.repro_string());
        record_powerloss_failure(
            repro.clone(),
            PowerLossFailurePhase::Recovery,
            panic_message(payload.as_ref()),
        );
        print_active_skip_sync(harness_skip);
        eprintln!("POWERLOSS_LOG_FINGERPRINT={:016x}", log_fingerprint(info));
        panic!("POWERLOSS_REPRO={repro}");
    }
}

fn log_fingerprint(info: &CrashInfo) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut update = |value: &dyn std::fmt::Debug| {
        for byte in format!("{value:?}").bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };

    for op in info.ops {
        update(op);
    }
    if let Some(recovery) = &info.recovery {
        for op in recovery.ops {
            update(op);
        }
    }
    update(&info.acked_labels);
    hash
}

fn print_active_skip_sync(harness_skip: &SkipSync) {
    let skip = effective_skip_sync(harness_skip);
    let spec = match skip {
        SkipSync::None => return,
        SkipSync::File => "file".to_owned(),
        SkipSync::Directory => "dir".to_owned(),
        SkipSync::All => "all".to_owned(),
        SkipSync::Site(site) => format!("site:{site}"),
    };
    eprintln!("POWERLOSS_SKIP_SYNC={spec}");
}

pub fn assert_skip_kills<F>(label: &str, skip: SkipSync, body: F)
where
    F: FnOnce(),
{
    clear_powerloss_failure();

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        htap_common::fs::with_skip_sync(skip, body);
    }));
    let hits = scope_skip_hits();
    let failure = powerloss_failure();
    clear_powerloss_failure();

    match result {
        Ok(()) => panic!("{label}: mutation survived"),
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            let Some(failure) = failure else {
                panic!("{label}: died for another reason: {message}");
            };
            if message != format!("POWERLOSS_REPRO={}", failure.repro) {
                panic!("{label}: died for another reason: {message}");
            }
            if hits == 0 {
                panic!("{label}: site never exercised");
            }
        }
    }
}

pub fn assert_skip_survives<F>(label: &str, skip: SkipSync, body: F)
where
    F: FnOnce(),
{
    clear_powerloss_failure();

    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        htap_common::fs::with_skip_sync(skip, body);
    }));
    let hits = scope_skip_hits();
    clear_powerloss_failure();

    if let Err(payload) = result {
        panic!("{label}: {}", panic_message(payload.as_ref()));
    }

    if hits == 0 {
        panic!("{label}: site never exercised");
    }
}

fn record_powerloss_failure(repro: String, phase: PowerLossFailurePhase, message: String) {
    if is_fail_closed_panic(&message) {
        clear_powerloss_failure();
        return;
    }

    set_powerloss_failure(PowerLossFailure {
        repro,
        phase,
        message,
    });
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn is_fail_closed_panic(message: &str) -> bool {
    [
        "registered root",
        "crash model",
        "crashsim",
        "raw std::fs",
        "modeled file contents",
        "path resolved differently than recorded",
        "recorder disappeared while resolving",
    ]
    .iter()
    .any(|denied| message.contains(denied))
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
