//! Folder syncs that run on their own.
//!
//! ## A schedule authorises a policy, not a set
//!
//! Everything else that writes to disk in this app is authorised by a
//! [`rdirstat_core::ConfirmationToken`], which means "a human looked at this
//! exact file set and these exact counts, moments ago". A schedule cannot mean
//! that and must not borrow the mechanism: the entire purpose of scheduling is
//! that the set at run time differs from anything a human reviewed. What the
//! user authorised is narrower and more durable — **this source, this
//! destination, additive only** — and [`crate::sync::apply_scheduled`] is the
//! entry point that accepts that kind of authority and no token at all.
//!
//! ## A stored path is not authority
//!
//! The dangerous failure here is not a wrong file, it is a right file on the
//! wrong disk. Between Tuesday and Saturday a destination can become a symlink,
//! a stale mount point, or — the bad one — an ordinary empty directory where a
//! volume used to be mounted. `/Volumes/Backup` with nothing mounted on it is a
//! perfectly writable folder on the boot disk, so a schedule pointed there
//! cheerfully fills the startup volume with hundreds of gigabytes while the
//! disk the user meant sits in a drawer.
//!
//! So a schedule records the **device id** of both endpoints when it is saved,
//! and re-checks them immediately before every run. A mismatch is a refusal,
//! not a warning: nobody is watching at 03:00, and a warning nobody reads is
//! indistinguishable from consent.
//!
//! ## Additive is not the same as harmless
//!
//! Nothing here deletes or overwrites. It still consumes space on the far side,
//! still pushes files somewhere that was configured months ago and may have
//! been forgotten, and still saturates a metered or mobile link. None of that
//! is destruction and all of it is a surprise, which is why the UI says so
//! rather than leaning on "additive" as though it settled the question.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use rdirstat_core::TreeGeneration;

use crate::sync::CompareMode;

/// The shortest interval a schedule may have.
///
/// Not a policy about taste — a floor that stops a schedule from overlapping
/// itself. A sync over a large tree can take longer than a minute, and a
/// one-minute interval would start the next run while the last is still
/// copying.
pub(crate) const MINIMUM_INTERVAL_MINUTES: u32 = 15;

/// How many run records are kept per schedule.
const HISTORY_LIMIT: usize = 20;

/// What happened the last time a schedule ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "detail")]
pub(crate) enum RunOutcome {
    /// Files were copied, or there was nothing to copy. Carries the count.
    Copied { files: u64, bytes: u64 },
    /// Refused before touching anything, with the reason.
    Refused(String),
    /// The sync ran and reported per-file failures.
    Failed(String),
}

/// One entry in a schedule's own log.
///
/// Kept so that when something copies 400 GB at 03:00 the user can find out
/// what did it. A record that said "confirmed" would be a lie — the whole
/// point is that nobody confirmed anything at 03:00 — so these name the
/// schedule and nothing else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct RunRecord {
    pub at_unix_ms: i64,
    pub outcome: RunOutcome,
}

/// A saved, unattended sync.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncSchedule {
    /// Stable across edits, so run history survives a rename.
    pub id: String,
    pub name: String,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub compare_mode: CompareMode,
    pub every_minutes: u32,
    pub enabled: bool,
    /// The device ids observed when the schedule was saved. `None` for a
    /// schedule written before this existed, which is treated as "unverified"
    /// and re-recorded on the next successful run rather than as "matches".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_device: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<RunRecord>,
}

impl SyncSchedule {
    /// True when `now` is at or past this schedule's next due time.
    ///
    /// A schedule that has never run is due immediately. That is deliberate:
    /// the alternative is a schedule the user just created sitting inert for
    /// up to its whole interval with no way to tell it apart from a broken
    /// one.
    pub(crate) fn is_due(&self, now_unix_ms: i64) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(last) = self.last_run_unix_ms else {
            return true;
        };
        let interval_ms = i64::from(self.every_minutes.max(MINIMUM_INTERVAL_MINUTES)) * 60_000;
        // Saturating, so a clock that jumped backwards produces "not yet"
        // rather than an overflow or a run every tick.
        now_unix_ms.saturating_sub(last) >= interval_ms
    }

    /// Records an outcome, trimming the history to its limit.
    pub(crate) fn record(&mut self, at_unix_ms: i64, outcome: RunOutcome) {
        self.last_run_unix_ms = Some(at_unix_ms);
        self.history.insert(0, RunRecord { at_unix_ms, outcome });
        self.history.truncate(HISTORY_LIMIT);
    }
}

/// Why a schedule was refused before it ran.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A path is gone, or is not a directory any more.
    Missing { path: PathBuf },
    /// The path exists but is on a different volume than when it was saved.
    ///
    /// The unmounted-mount-point case, and the reason this check exists.
    WrongVolume { path: PathBuf, saved: u64, found: u64 },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { path } => {
                write!(f, "{} is not there, or is not a folder", path.display())
            }
            Self::WrongVolume { path, .. } => write!(
                f,
                "{} is on a different disk than when this was set up — if that volume is not mounted, \
                 syncing now would fill the startup disk instead",
                path.display()
            ),
        }
    }
}

/// Checks that both endpoints are still what the schedule was saved against.
///
/// Runs immediately before every run, never only at save time. The whole class
/// of bug this defends against happens *between* those two moments.
///
/// # Errors
///
/// [`Refusal`] naming the endpoint at fault.
pub(crate) fn verify_endpoints(schedule: &SyncSchedule) -> Result<(), Refusal> {
    for (path, saved) in [
        (schedule.source.as_path(), schedule.source_device),
        (schedule.destination.as_path(), schedule.destination_device),
    ] {
        if !path.is_dir() {
            return Err(Refusal::Missing {
                path: path.to_path_buf(),
            });
        }
        let Ok(observed) = crate::fsident::observe(path) else {
            return Err(Refusal::Missing {
                path: path.to_path_buf(),
            });
        };
        // `None` means the schedule predates device recording. Treated as
        // unverified rather than as a match: refusing every old schedule
        // would be hostile, and claiming they match would be a lie. They get
        // their device recorded on the next successful run.
        if let Some(saved) = saved
            && saved != observed.device
        {
            return Err(Refusal::WrongVolume {
                path: path.to_path_buf(),
                saved,
                found: observed.device,
            });
        }
    }
    Ok(())
}

/// Rejects a pair a schedule must never be saved with.
///
/// Checked at save time *as well as* at run time, not instead of it. This one
/// catches a typo while the user is looking at the form; [`verify_endpoints`]
/// catches the world changing afterwards, which is the failure nobody is
/// present for.
///
/// # Errors
///
/// A human-readable reason, rendered next to the field that caused it.
pub(crate) fn validate(source: &Path, destination: &Path) -> Result<(), String> {
    for path in [source, destination] {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(format!(
                "{} must be an absolute path with no `..` segments",
                path.display()
            ));
        }
        if !path.is_dir() {
            return Err(format!("{} is not a folder", path.display()));
        }
    }

    // Resolved, not lexical: two paths sharing no components can name the same
    // directory through a symlink, and a schedule syncing a tree into itself
    // would walk its own output every hour, for ever.
    let overlapping = |a: &Path, b: &Path| a == b || b.starts_with(a) || a.starts_with(b);
    let same = overlapping(source, destination)
        || match (source.canonicalize(), destination.canonicalize()) {
            (Ok(left), Ok(right)) => overlapping(&left, &right),
            _ => false,
        };
    if same {
        return Err("the two folders overlap — one contains the other, or they are the same folder".to_owned());
    }
    Ok(())
}

/// The device id of a path, for recording at save time.
pub(crate) fn device_of(path: &Path) -> Option<u64> {
    crate::fsident::observe(path).ok().map(|observed| observed.device)
}

/// Runs every schedule that is due, and records what happened.
///
/// Returns how many actually ran, which is what the caller logs.
///
/// **Re-reads the settings file before writing each result** rather than saving
/// the whole collection it started from. A sync can take minutes, and the user
/// may add, edit or disable a schedule while one is running; saving a stale
/// snapshot would silently undo that edit. So the run result is merged into
/// whatever is on disk *now*, by id, and a schedule that was deleted mid-run
/// simply has nowhere to merge into.
pub(crate) fn run_due(app_data: &Path, now_unix_ms: i64) -> usize {
    let due: Vec<SyncSchedule> = crate::settings::load(app_data)
        .schedules
        .into_iter()
        .filter(|schedule| schedule.is_due(now_unix_ms))
        .collect();

    let mut ran = 0;
    for schedule in due {
        let outcome = run_one(&schedule);
        ran += 1;
        merge_result(app_data, &schedule.id, now_unix_ms, outcome);
    }
    ran
}

/// Verifies a single schedule's endpoints and, if they hold, syncs it.
///
/// The verification is not optional and not a warning. Nobody is watching at
/// 03:00, so a refusal is the only safe shape for a check that fails.
pub(crate) fn run_one(schedule: &SyncSchedule) -> RunOutcome {
    if let Err(refusal) = verify_endpoints(schedule) {
        tracing::warn!(
            schedule = %schedule.id,
            name = %schedule.name,
            reason = %refusal,
            "scheduled sync refused"
        );
        return RunOutcome::Refused(refusal.to_string());
    }

    // Named in the log, and named as a SCHEDULE. An audit line that said
    // "confirmed" would be false — nobody confirmed anything — and would make
    // an automated run indistinguishable from a human one afterwards.
    tracing::info!(
        schedule = %schedule.id,
        name = %schedule.name,
        source = %schedule.source.display(),
        destination = %schedule.destination.display(),
        "scheduled sync starting"
    );

    match crate::sync::apply_scheduled(
        TreeGeneration::FIRST,
        &schedule.source,
        &schedule.destination,
        schedule.compare_mode,
    ) {
        Err(error) => RunOutcome::Refused(error.to_string()),
        Ok(report) if report.failures.is_empty() => RunOutcome::Copied {
            files: report.copied,
            bytes: report.bytes_copied,
        },
        Ok(report) => RunOutcome::Failed(format!(
            "{} copied, {} failed — first: {}",
            report.copied,
            report.failures.len(),
            report
                .failures
                .first()
                .map_or("unknown", |failure| failure.reason.as_str())
        )),
    }
}

/// Runs one schedule immediately, whether or not it is due.
///
/// Deliberately the same path as [`run_due`], verification included: a "run
/// now" that skipped the endpoint checks would be a different operation
/// wearing the same name, and would tell the user nothing about whether the
/// unattended version is going to work.
pub(crate) fn run_now(app_data: &Path, id: &str, now_unix_ms: i64) {
    let Some(schedule) = crate::settings::load(app_data)
        .schedules
        .into_iter()
        .find(|schedule| schedule.id == id)
    else {
        return;
    };
    let outcome = run_one(&schedule);
    merge_result(app_data, id, now_unix_ms, outcome);
}

/// Merges one run's result into the settings file as it stands now.
fn merge_result(app_data: &Path, id: &str, now_unix_ms: i64, outcome: RunOutcome) {
    let mut settings = crate::settings::load(app_data);
    let Some(schedule) = settings.schedules.iter_mut().find(|schedule| schedule.id == id) else {
        return; // Deleted while it ran. Nothing to record it against.
    };
    // A schedule saved before device recording existed gets its devices filled
    // in by its first successful run, so the check starts protecting it.
    if matches!(outcome, RunOutcome::Copied { .. }) {
        if schedule.source_device.is_none() {
            schedule.source_device = device_of(&schedule.source);
        }
        if schedule.destination_device.is_none() {
            schedule.destination_device = device_of(&schedule.destination);
        }
    }
    schedule.record(now_unix_ms, outcome);
    if let Err(error) = crate::settings::save(app_data, &settings) {
        tracing::warn!(%error, "could not record a scheduled run");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule() -> SyncSchedule {
        SyncSchedule {
            id: "s1".to_owned(),
            name: "Photos".to_owned(),
            source: PathBuf::from("/a"),
            destination: PathBuf::from("/b"),
            compare_mode: CompareMode::Quick,
            every_minutes: 60,
            enabled: true,
            source_device: None,
            destination_device: None,
            last_run_unix_ms: None,
            history: Vec::new(),
        }
    }

    #[test]
    fn a_new_schedule_is_due_at_once() {
        assert!(schedule().is_due(1_000));
    }

    #[test]
    fn a_disabled_schedule_is_never_due() {
        let mut s = schedule();
        s.enabled = false;
        assert!(!s.is_due(i64::MAX));
    }

    #[test]
    fn a_schedule_is_due_again_only_after_its_interval() {
        let mut s = schedule();
        s.last_run_unix_ms = Some(1_000_000);
        assert!(!s.is_due(1_000_000 + 59 * 60_000));
        assert!(s.is_due(1_000_000 + 60 * 60_000));
    }

    /// A too-short interval must not let a run overlap itself.
    #[test]
    fn an_interval_below_the_floor_is_treated_as_the_floor() {
        let mut s = schedule();
        s.every_minutes = 1;
        s.last_run_unix_ms = Some(0);
        assert!(!s.is_due(i64::from(MINIMUM_INTERVAL_MINUTES) * 60_000 - 1));
        assert!(s.is_due(i64::from(MINIMUM_INTERVAL_MINUTES) * 60_000));
    }

    /// A clock that jumped backwards must not fire every tick.
    #[test]
    fn a_backwards_clock_does_not_make_everything_due() {
        let mut s = schedule();
        s.last_run_unix_ms = Some(10_000_000);
        assert!(!s.is_due(1_000));
    }

    #[test]
    fn history_is_newest_first_and_bounded() {
        let mut s = schedule();
        let limit = i64::try_from(HISTORY_LIMIT).expect("a small constant fits an i64");
        for tick in 0..(limit + 5) {
            s.record(tick, RunOutcome::Copied { files: 1, bytes: 2 });
        }
        assert_eq!(s.history.len(), HISTORY_LIMIT);
        assert_eq!(s.history[0].at_unix_ms, limit + 4);
        assert_eq!(s.last_run_unix_ms, Some(limit + 4));
    }

    #[test]
    fn a_missing_endpoint_is_refused() {
        let mut s = schedule();
        s.source = PathBuf::from("/definitely/not/here");
        assert!(matches!(verify_endpoints(&s), Err(Refusal::Missing { .. })));
    }

    /// The unmounted-mount-point case: the folder exists and is writable, but
    /// it is not the disk the user meant.
    #[test]
    fn a_path_on_a_different_volume_than_recorded_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let real = device_of(dir.path()).expect("device");
        let mut s = schedule();
        s.source = dir.path().to_path_buf();
        s.destination = dir.path().to_path_buf();
        s.source_device = Some(real);
        s.destination_device = Some(real.wrapping_add(1));

        match verify_endpoints(&s) {
            Err(Refusal::WrongVolume { found, .. }) => assert_eq!(found, real),
            other => panic!("expected a wrong-volume refusal, got {other:?}"),
        }
    }

    /// A schedule saved before device recording existed must still run.
    #[test]
    fn an_unrecorded_device_is_not_treated_as_a_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut s = schedule();
        s.source = dir.path().to_path_buf();
        s.destination = dir.path().to_path_buf();
        assert_eq!(verify_endpoints(&s), Ok(()));
    }
}
