use std::fmt;

use htap_common::fs::Op;

/// Defines how unforced filesystem changes are represented after a crash.
#[derive(Clone)]
pub enum CrashPolicy {
    /// Only explicitly forced operations persist.
    Strict,
    /// Metadata is strict; dirty file sectors may selectively persist.
    Torn { seed: u64, sector_size: usize },
    /// Unforced directory operations and dirty sectors persist independently.
    Chaos { seed: u64 },
}

/// Describes the recovery stage of a generated crash image.
pub struct RecoveryStage<'a> {
    /// The operation index at which recovery crashes.
    pub crash_point: usize,
    /// Whether recovery completed instead of crashing at `crash_point`.
    pub completed: bool,
    /// Operations observed during recovery before the crash point, in order.
    pub ops: &'a [Op],
    /// Acknowledgement labels observed during recovery before the crash point.
    pub acked_labels: Vec<String>,
}

/// Describes a generated crash image.
pub struct CrashInfo<'a> {
    /// The name of the harness that generated the image.
    pub harness_name: &'a str,
    /// The operation index at which the outer crash occurs.
    pub crash_point: usize,
    /// The policy used to generate the outer crash image.
    pub policy: CrashPolicy,
    /// Operations observed before the outer crash point, in order.
    pub ops: &'a [Op],
    /// Acknowledgement labels observed before the outer crash point, in order.
    pub acked_labels: Vec<String>,
    /// Recovery-stage details, if this image was generated during recovery.
    pub recovery: Option<RecoveryStage<'a>>,
}

impl CrashInfo<'_> {
    /// Returns a reproduction string suitable for `POWERLOSS_REPRO`.
    pub fn repro_string(&self) -> String {
        let variant = match &self.policy {
            CrashPolicy::Torn { sector_size, .. } => format!("torn-s={sector_size}"),
            _ => self.policy.short_name().to_owned(),
        };
        let seed = match &self.policy {
            CrashPolicy::Strict => 0,
            CrashPolicy::Torn { seed, .. } | CrashPolicy::Chaos { seed } => *seed,
        };

        let repro = format!(
            "{}/{}/{}/k={}",
            self.harness_name, variant, seed, self.crash_point
        );

        match &self.recovery {
            Some(recovery) if recovery.completed => format!("{repro}/rk=done"),
            Some(recovery) => format!("{repro}/rk={}", recovery.crash_point),
            None => repro,
        }
    }
}

impl CrashPolicy {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Torn { .. } => "Torn",
            Self::Chaos { .. } => "Chaos",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Torn { .. } => "torn",
            Self::Chaos { .. } => "chaos",
        }
    }
}

impl fmt::Debug for CrashPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Strict => f.write_str("Strict"),
            Self::Torn { seed, sector_size } => f
                .debug_struct("Torn")
                .field("seed", seed)
                .field("sector_size", sector_size)
                .finish(),
            Self::Chaos { seed } => f.debug_struct("Chaos").field("seed", seed).finish(),
        }
    }
}

impl fmt::Debug for RecoveryStage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryStage")
            .field("crash_point", &self.crash_point)
            .field("completed", &self.completed)
            .field("ops", &self.ops)
            .field("acked_labels", &self.acked_labels)
            .finish()
    }
}

impl fmt::Debug for CrashInfo<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrashInfo")
            .field("crash_point", &self.crash_point)
            .field("policy", &self.policy)
            .field("ops", &self.ops)
            .field("acked_labels", &self.acked_labels)
            .field("recovery", &self.recovery)
            .finish()
    }
}

/// Identifies whether recovery crashes at an operation or completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReproPoint {
    Crash(usize),
    Done,
    Recover,
}

/// Parses `test_name/policy_variant/seed/k=N` reproduction strings.
pub fn parse_repro_string(s: &str) -> Option<(String, String, u64, usize)> {
    let mut parts = s.split('/');

    let test_name = parts.next()?;
    let policy_variant = parts.next()?;
    let seed = parts.next()?.parse().ok()?;
    let crash_point = parts.next()?.strip_prefix("k=")?.parse().ok()?;

    if test_name.is_empty() || policy_variant.is_empty() || parts.next().is_some() {
        return None;
    }

    Some((
        test_name.to_owned(),
        policy_variant.to_owned(),
        seed,
        crash_point,
    ))
}

/// Parses `test_name/policy_variant/seed/k=N/rk=N|done|recover` reproduction strings.
pub fn parse_recovery_repro_string(
    s: &str,
) -> Option<(String, String, u64, usize, RecoveryReproPoint)> {
    let mut parts = s.split('/');

    let test_name = parts.next()?;
    let policy_variant = parts.next()?;
    let seed = parts.next()?.parse().ok()?;
    let crash_point = parts.next()?.strip_prefix("k=")?.parse().ok()?;
    let recovery_point = match parts.next()?.strip_prefix("rk=")? {
        "done" => RecoveryReproPoint::Done,
        "recover" => RecoveryReproPoint::Recover,
        value => RecoveryReproPoint::Crash(value.parse().ok()?),
    };

    if test_name.is_empty() || policy_variant.is_empty() || parts.next().is_some() {
        return None;
    }

    Some((
        test_name.to_owned(),
        policy_variant.to_owned(),
        seed,
        crash_point,
        recovery_point,
    ))
}
