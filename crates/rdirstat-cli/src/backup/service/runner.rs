//! Runs one job to an end: per label, rounds of `build_plan_scoped` + `apply_with`
//! until a round lands everything, lands nothing new, or the label runs out
//! of rounds; then the drive's index is rebuilt.
//!
//! This is `resume-loop.sh` in-process. A resume is a new plan: plan reads
//! the drive's own receipts, so what already landed is `already_landed`, not
//! copied again. A round that a lost volume cut short waits for the volume
//! and does not count against the label's rounds. Nothing on the
//! destination is ever deleted.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rdirstat_jobs::{
    Control, EventBody, ItemOutcome, JOB_FILE, JobSource, JobSpec, LabelStatus, PLANS_DIR, Phase, Tally,
};
use rdirstat_organize::{PlanItem, Rules};

use crate::backup::{ApplyHooks, ApplyRequest, DiskProbe, Gate, ItemResult, Outcome, PlanRequest, apply_with, now_ms};

use super::record::Record;

/// How often a held (paused) run, or a wait, looks at its control word and
/// the shutdown flag.
const TICK: Duration = Duration::from_millis(100);

/// Called after each item with the job id and result. Tests use it to act at
/// an exact item boundary.
pub(crate) type ItemHook = dyn Fn(&str, &ItemResult) + Send + Sync;

/// Everything a job run needs.
pub(crate) struct Runner<'a> {
    pub(crate) spec: &'a JobSpec,
    pub(crate) record: &'a Mutex<Record>,
    pub(crate) probe: &'a dyn DiskProbe,
    pub(crate) shutdown: &'a AtomicBool,
    /// Catalog for a job whose spec names none.
    pub(crate) default_catalog: Option<PathBuf>,
    /// The app's own catalog directory (see `refuse_write_under_source`).
    pub(crate) app_catalog: Option<PathBuf>,
    pub(crate) on_item: Option<&'a ItemHook>,
}

impl std::fmt::Debug for Runner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("job", &self.spec.id)
            .finish_non_exhaustive()
    }
}

/// Why a run stopped before its end.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Halt {
    Cancel,
    Shutdown,
}

impl Halt {
    fn reason(&self) -> String {
        match self {
            Self::Cancel => "cancelled".to_owned(),
            Self::Shutdown => "the backup service is shutting down".to_owned(),
        }
    }
}

/// A wait for a volume that ran out of time.
struct TimedOut(String);

impl Runner<'_> {
    /// Runs the job and returns the phase it ended in.
    pub(crate) fn run(&self) -> Phase {
        {
            let mut record = self.lock();
            let state = record.state_mut();
            state.started_at_ms.get_or_insert_with(now_ms);
            state.finished_at_ms = None;
        }
        for (index, source) in self.spec.sources.iter().enumerate() {
            let status = self.lock().state().labels.get(index).map(|label| label.status);
            if matches!(
                status,
                Some(LabelStatus::Done | LabelStatus::Incomplete | LabelStatus::Failed)
            ) {
                continue;
            }
            if let Err(halt) = self.run_label(index, source) {
                return self.end(halt_phase(&halt), Some(halt.reason()));
            }
        }
        if let Err(halt) = self.index() {
            return self.end(halt_phase(&halt), Some(halt.reason()));
        }
        let statuses: Vec<LabelStatus> = self.lock().state().labels.iter().map(|label| label.status).collect();
        let phase = if statuses.contains(&LabelStatus::Failed) {
            Phase::Failed
        } else if statuses.iter().all(|status| *status == LabelStatus::Done) {
            Phase::Done
        } else {
            Phase::Incomplete
        };
        self.end(phase, None)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn phase(&self, phase: Phase, label: Option<&str>, round: u32, detail: Option<String>) {
        let mut record = self.lock();
        let state = record.state_mut();
        state.phase = phase;
        if label.is_some() {
            state.label = label.map(str::to_owned);
        }
        state.round = round;
        record.emit(EventBody::Phase {
            phase,
            label: label.map(str::to_owned),
            round,
            detail,
        });
    }

    fn error(&self, label: Option<&str>, message: String) {
        let mut record = self.lock();
        record.state_mut().last_error = Some(message.clone());
        record.emit(EventBody::Error {
            label: label.map(str::to_owned),
            message,
        });
    }

    fn end(&self, phase: Phase, detail: Option<String>) -> Phase {
        {
            let mut record = self.lock();
            let state = record.state_mut();
            state.current_item = None;
            if phase.is_terminal() {
                state.finished_at_ms = Some(now_ms());
            }
        }
        let (label, round) = {
            let record = self.lock();
            (record.state().label.clone(), record.state().round)
        };
        self.phase(phase, label.as_deref(), round, detail);
        phase
    }

    fn set_label(&self, index: usize, change: impl FnOnce(&mut rdirstat_jobs::LabelProgress)) {
        let mut record = self.lock();
        if let Some(label) = record.state_mut().labels.get_mut(index) {
            change(label);
        }
        record.save_state(true);
    }

    fn run_label(&self, index: usize, source: &JobSource) -> Result<(), Halt> {
        let label = source.label.as_str();
        loop {
            let (rounds, drops) = {
                let record = self.lock();
                record
                    .state()
                    .labels
                    .get(index)
                    .map_or((0, 0), |progress| (progress.rounds, progress.drops))
            };
            if rounds.saturating_sub(drops) >= self.spec.max_rounds {
                self.set_label(index, |progress| progress.status = LabelStatus::Incomplete);
                return Ok(());
            }
            self.checkpoint(label, rounds)?;
            if let Err(TimedOut(why)) = self.wait_for_mounts(Some(source), label, rounds)? {
                self.error(Some(label), why);
                self.set_label(index, |progress| progress.status = LabelStatus::Failed);
                return Ok(());
            }
            let round = rounds + 1;
            self.set_label(index, |progress| {
                progress.rounds = round;
                progress.status = LabelStatus::Running;
            });
            {
                let mut record = self.lock();
                let state = record.state_mut();
                state.items_done = 0;
                state.items_total = 0;
            }
            self.phase(Phase::Planning, Some(label), round, None);
            let plan = match self.plan(source, round) {
                Ok(plan) => plan,
                Err(error) => {
                    if self.round_failed(index, source, &format!("planning {label}: {error:#}")) {
                        continue;
                    }
                    return Ok(());
                }
            };
            self.set_label(index, |progress| progress.last_plan = Some(plan.plan_path.clone()));
            {
                let mut record = self.lock();
                record.state_mut().items_total = plan.plan.items.len() as u64;
            }
            self.checkpoint(label, round)?;
            self.phase(Phase::Applying, Some(label), round, None);
            let mut hooks = RoundHooks {
                runner: self,
                label,
                round,
                halt: None,
            };
            let request = ApplyRequest {
                plan: plan.plan_path.clone(),
                token: plan.token.clone(),
                catalog: self.catalog(),
                app_catalog: self.app_catalog.clone(),
                progress: false,
            };
            let report = match apply_with(&request, self.probe, &mut hooks) {
                Ok(report) => report,
                Err(error) => {
                    if self.round_failed(index, source, &format!("applying {label}: {error:#}")) {
                        continue;
                    }
                    return Ok(());
                }
            };
            let halt = hooks.halt.take();
            let lost_volume = report.stopped.is_some() && halt.is_none();
            let landed = self.record_round(index, label, round, &report, lost_volume);
            let complete = report.complete();
            if let Some(halt) = halt {
                return Err(halt);
            }
            if complete {
                self.set_label(index, |progress| progress.status = LabelStatus::Done);
                return Ok(());
            }
            if lost_volume {
                continue;
            }
            if landed.items == 0 {
                self.set_label(index, |progress| progress.status = LabelStatus::Incomplete);
                return Ok(());
            }
        }
    }

    /// Adds a finished round's outcomes to the label and emits its `round`
    /// event. Returns what it copied or cloned.
    fn record_round(
        &self,
        index: usize,
        label: &str,
        round: u32,
        report: &crate::backup::ApplyReport,
        lost_volume: bool,
    ) -> Tally {
        let mut landed = Tally::default();
        let mut already = Tally::default();
        let mut not_landed = Tally::default();
        for result in &report.results {
            let tally = match map_outcome(result.outcome) {
                outcome if outcome.landed_now() => &mut landed,
                ItemOutcome::AlreadyLanded => &mut already,
                _ => &mut not_landed,
            };
            tally.items += 1;
            tally.bytes += result.bytes;
        }
        let mut record = self.lock();
        if let Some(error) = &report.catalog_error {
            record.state_mut().last_error = Some(error.clone());
        }
        if let Some(progress) = record.state_mut().labels.get_mut(index) {
            progress.landed_items += landed.items;
            progress.landed_bytes += landed.bytes;
            progress.not_landed_items = not_landed.items;
            if lost_volume {
                progress.drops += 1;
            }
        }
        record.state_mut().current_item = None;
        record.emit(EventBody::Round {
            label: label.to_owned(),
            round,
            landed,
            already_landed: already,
            not_landed,
            complete: report.complete(),
            stopped: report.stopped.clone(),
        });
        landed
    }

    /// Records a refused plan or apply. If a volume is missing now, the round
    /// is a drop and the label goes on (`true`); otherwise the label failed.
    fn round_failed(&self, index: usize, source: &JobSource, message: &str) -> bool {
        self.error(Some(&source.label), message.to_owned());
        if self.missing(Some(source)).is_empty() {
            self.set_label(index, |progress| progress.status = LabelStatus::Failed);
            false
        } else {
            self.set_label(index, |progress| progress.drops += 1);
            true
        }
    }

    fn catalog(&self) -> PathBuf {
        self.spec
            .catalog
            .clone()
            .or_else(|| self.default_catalog.clone())
            .unwrap_or_else(|| self.lock().dir().join("catalog"))
    }

    fn plan(&self, source: &JobSource, round: u32) -> anyhow::Result<crate::backup::PlanOutcome> {
        use anyhow::Context as _;
        let dir = self.lock().dir().to_path_buf();
        let rules = match &self.spec.rules {
            Some(path) => {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    dir.join(path)
                };
                let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
                Rules::from_toml(&text).with_context(|| format!("rules file {}", path.display()))?
            }
            None => Rules::default_backup(),
        };
        let plans = dir.join(PLANS_DIR);
        match std::fs::create_dir(&plans) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).with_context(|| format!("cannot create {}", plans.display())),
        }
        let request = PlanRequest {
            source: Some(source.root.clone()),
            label: source.label.clone(),
            dest: self.spec.dest.clone(),
            floor: self.spec.floor,
            snapshot: None,
            rules,
            out: Some(plans.join(format!("{}-r{round}.json", source.label))),
            catalog: self.catalog(),
            app_catalog: self.app_catalog.clone(),
            progress: false,
            rollup: self.spec.rollup,
        };
        let scope = crate::backup::plan::PlanScope {
            includes: source.include.clone(),
            cap: self.spec.review_cap.unwrap_or(rdirstat_organize::DEFAULT_CAP),
            dry_run: false,
        };
        crate::backup::plan::build_plan_scoped(&request, &scope, self.probe)
    }

    fn index(&self) -> Result<(), Halt> {
        let (label, round) = {
            let record = self.lock();
            (record.state().label.clone(), record.state().round)
        };
        loop {
            self.checkpoint(label.as_deref().unwrap_or_default(), round)?;
            if let Err(TimedOut(why)) = self.wait_for_mounts(None, label.as_deref().unwrap_or_default(), round)? {
                self.error(None, why);
                return Ok(());
            }
            if !rdirstat_catalog::dest_catalog_dir(&self.spec.dest).is_dir() {
                // Nothing has ever landed on this drive: no index to build.
                return Ok(());
            }
            self.phase(Phase::Indexing, label.as_deref(), round, None);
            match rdirstat_catalog::rebuild_index(&self.spec.dest) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    self.error(None, format!("rebuilding the index: {error}"));
                    if self.missing(None).is_empty() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// What is missing: the source root (when given) and the destination,
    /// which is judged by this job's own `job.json` being at its path, so a
    /// bare mount point left behind by a dropped drive does not count.
    fn missing(&self, source: Option<&JobSource>) -> Vec<String> {
        let mut missing = Vec::new();
        if let Some(source) = source
            && std::fs::read_dir(&source.root).is_err()
        {
            missing.push(source.root.display().to_string());
        }
        let marker = self.lock().dir().join(JOB_FILE);
        if !marker.is_file() {
            missing.push(self.spec.dest.display().to_string());
        }
        missing
    }

    /// Waits until nothing is [`Runner::missing`], announcing the wait once.
    /// The outer `Err` is a halt; the inner one a timeout.
    fn wait_for_mounts(
        &self,
        source: Option<&JobSource>,
        label: &str,
        round: u32,
    ) -> Result<Result<(), TimedOut>, Halt> {
        let started = Instant::now();
        let mut announced = false;
        loop {
            let missing = self.missing(source);
            if missing.is_empty() {
                if announced {
                    self.lock().flush();
                }
                return Ok(Ok(()));
            }
            let what = missing.join(" and ");
            if !announced {
                announced = true;
                self.phase(
                    Phase::WaitingForMount,
                    (!label.is_empty()).then_some(label),
                    round,
                    Some(format!("waiting for {what}")),
                );
            }
            if let Some(timeout) = self.spec.mount_timeout_ms
                && started.elapsed() >= Duration::from_millis(timeout)
            {
                return Ok(Err(TimedOut(format!("{what} did not come back within {timeout} ms"))));
            }
            self.sleep(Duration::from_millis(self.spec.mount_poll_ms.max(1)))?;
        }
    }

    /// Sleeps `total`, waking early for shutdown or cancel.
    fn sleep(&self, total: Duration) -> Result<(), Halt> {
        let until = Instant::now() + total;
        loop {
            self.halted()?;
            let now = Instant::now();
            if now >= until {
                return Ok(());
            }
            std::thread::sleep(TICK.min(until - now));
        }
    }

    fn halted(&self) -> Result<(), Halt> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(Halt::Shutdown);
        }
        if self.lock().control() == Control::Cancel {
            return Err(Halt::Cancel);
        }
        Ok(())
    }

    /// Between phases: stops on shutdown or cancel, holds while paused.
    fn checkpoint(&self, label: &str, round: u32) -> Result<(), Halt> {
        self.halted()?;
        if self.lock().control() == Control::Pause {
            self.hold(label, round)?;
        }
        Ok(())
    }

    /// Holds while the control word is `pause`.
    fn hold(&self, label: &str, round: u32) -> Result<(), Halt> {
        let before = self.lock().state().phase;
        self.phase(Phase::Paused, (!label.is_empty()).then_some(label), round, None);
        loop {
            self.halted()?;
            if self.lock().control() != Control::Pause {
                let back = if before == Phase::Paused {
                    Phase::Applying
                } else {
                    before
                };
                self.phase(back, (!label.is_empty()).then_some(label), round, None);
                return Ok(());
            }
            std::thread::sleep(TICK);
        }
    }
}

const fn halt_phase(halt: &Halt) -> Phase {
    match halt {
        Halt::Cancel => Phase::Cancelled,
        Halt::Shutdown => Phase::Stopped,
    }
}

/// `backup apply`'s outcome as the wire contract names it.
pub(crate) const fn map_outcome(outcome: Outcome) -> ItemOutcome {
    match outcome {
        Outcome::Copied => ItemOutcome::Copied,
        Outcome::Cloned => ItemOutcome::Cloned,
        Outcome::AlreadyLanded => ItemOutcome::AlreadyLanded,
        Outcome::ChangedSinceScan => ItemOutcome::ChangedSinceScan,
        Outcome::ChangedDuringCopy => ItemOutcome::ChangedDuringCopy,
        Outcome::DestinationOccupied => ItemOutcome::DestinationOccupied,
        Outcome::Failed => ItemOutcome::Failed,
    }
}

/// The apply hooks of one round.
struct RoundHooks<'r, 'a> {
    runner: &'r Runner<'a>,
    label: &'r str,
    round: u32,
    halt: Option<Halt>,
}

impl ApplyHooks for RoundHooks<'_, '_> {
    fn gate(&mut self, item: &PlanItem) -> Gate {
        if let Err(halt) = self.runner.halted() {
            let reason = halt.reason();
            self.halt = Some(halt);
            return Gate::Stop(reason);
        }
        let mut record = self.runner.lock();
        record.state_mut().current_item = Some(item.relative.to_string_lossy().into_owned());
        if record.control() == Control::Pause {
            Gate::Hold
        } else {
            Gate::Go
        }
    }

    fn hold(&mut self) -> Gate {
        match self.runner.hold(self.label, self.round) {
            Ok(()) => Gate::Go,
            Err(halt) => {
                let reason = halt.reason();
                self.halt = Some(halt);
                Gate::Stop(reason)
            }
        }
    }

    fn item_done(&mut self, index: usize, total: usize, result: &ItemResult) {
        let outcome = map_outcome(result.outcome);
        {
            let mut record = self.runner.lock();
            let state = record.state_mut();
            let tally = state.counts.entry(outcome).or_default();
            tally.items += 1;
            tally.bytes += result.bytes;
            state.items_done = index as u64 + 1;
            record.emit(EventBody::Item {
                label: self.label.to_owned(),
                round: self.round,
                index: index as u64 + 1,
                total: total as u64,
                relative: result.relative.clone(),
                dest: result.dest.clone(),
                outcome,
                bytes: result.bytes,
                detail: result.detail.clone(),
                partial: result.partial.clone(),
            });
        }
        if let Some(hook) = self.runner.on_item {
            hook(&self.runner.spec.id, result);
        }
    }
}
