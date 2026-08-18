//! The scan seam: `rdirstat-scan` driven on behalf of the command layer.
//!
//! This module used to *be* a scanner — a bounded work queue, N reader threads,
//! a single arena builder, and the whole symlink / mount / firmlink / hard-link
//! policy — written because `crates/rdirstat-scan` was still a stub. It is now
//! what it always said it would become: an adapter. [`run`] configures a
//! [`Scanner`] and translates its result; `commands.rs`, `state.rs`, and
//! `progress.rs` were not changed by the swap.
//!
//! Three things are bridged here, and only here.
//!
//! 1. **Cancellation.** [`crate::state::CancelToken`] owns a
//!    [`rdirstat_scan::CancelToken`] and sets both flags in `request()`, so
//!    this hands the scanner a clone rather than polling.
//! 2. **Progress.** [`CounterSink`] copies each `ScanProgress` snapshot into the
//!    [`ProgressCounters`] atomics the existing 10 Hz emitter already reads.
//!    Double throttling is harmless because every counter is absolute. Keeping
//!    the emitter is deliberate: it samples real process RSS with `/bin/ps`,
//!    and `ScanProgress::rss_bytes` from the scanner is *arena* bytes, not RSS.
//! 3. **Categorization.** [`ClassifyAdapter`] joins `rdirstat_scan::Categorizer`
//!    to `rdirstat_classify::Categorizer`. The two traits differ by one
//!    argument — the scanner offers its 16-bit node flags, the classifier wants
//!    `st_mode` — because neither crate may depend on the other's shape. This
//!    is the seam that finally gives every node a real category; before the
//!    swap every node in the app was `CategoryId::UNCATEGORIZED`.
//!
//! ## Behaviour that changed with the swap, and is not a regression to fix here
//!
//! - **`aggregate_below_bytes` is refused**, not honoured. The old module
//!   implemented aggregation; `rdirstat-scan` rejects the option with
//!   `StartError::InvalidOptions` rather than silently scanning at full detail.
//!   The frontend never sets it.
//! - **Default exclusions are prepended to the user's rules**, and first match
//!   wins, so a user `RuleAction::Include` cannot re-include a default-excluded
//!   path. The old module put user rules first. This follows the doc comment on
//!   `ScanOptions::apply_default_exclusions`; it is flagged in the scan crate's
//!   handoff as a product decision that wants a human.
//! - The default exclusion list is now docs/02's, not the old placeholder.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rdirstat_core::{
    CategoryId, CompletedScan, ConfigHash, DisplayPath, ExclusionRule, Kind, ScanError, ScanId, ScanOptions,
    ScanProgress, ScanState, StartError, TreeGeneration, flags,
};
use rdirstat_scan::{Categorizer as ScanCategorizer, ErrorSink, ProgressSink, Scanner};

use crate::progress::ProgressCounters;
use crate::state::CancelToken;

/// Everything one scan needs.
#[derive(Debug)]
pub(crate) struct ScanRequest {
    /// The validated scan root.
    pub root: PathBuf,
    /// The options in force.
    pub options: ScanOptions,
    /// The attempt id.
    pub scan_id: ScanId,
    /// The generation this scan will publish under.
    pub generation: TreeGeneration,
    /// Cooperative cancellation.
    pub cancel: Arc<CancelToken>,
    /// The atomics the 10 Hz emitter reads.
    pub counters: Arc<ProgressCounters>,
}

/// How a scan ended.
#[derive(Debug)]
pub(crate) enum ScanOutcome {
    /// A complete (possibly partial-by-permission) tree, ready to publish.
    Completed(Box<CompletedScan>),
    /// Cancelled. Nothing is published and the partial arena is dropped.
    Cancelled,
    /// A fatal failure. Nothing is published.
    Failed(ScanError),
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Mirrors the scanner's snapshots into the atomics the emitter thread reads.
struct CounterSink {
    counters: Arc<ProgressCounters>,
}

impl ProgressSink for CounterSink {
    fn publish(&self, progress: &ScanProgress) {
        let counters = &self.counters;
        counters
            .observed_entries
            .store(progress.observed_entries, Ordering::Relaxed);
        counters
            .retained_nodes
            .store(progress.retained_nodes, Ordering::Relaxed);
        counters.directories.store(progress.directories, Ordering::Relaxed);
        counters.logical_bytes.store(progress.logical_bytes, Ordering::Relaxed);
        counters
            .allocated_bytes
            .store(progress.allocated_bytes, Ordering::Relaxed);
        counters.errors.store(progress.errors, Ordering::Relaxed);
        counters.mutations.store(progress.mutations, Ordering::Relaxed);
        counters.pending_dirs.store(progress.pending_dirs, Ordering::Relaxed);
        counters
            .result_queue_depth
            .store(progress.result_queue_depth, Ordering::Relaxed);

        // The snapshot carries the *mean* name length; the counters hold the
        // total, and rebuild the mean the same way. Reconstructing the total is
        // exact to within the integer division the scanner already performed.
        counters.name_bytes.store(
            u64::from(progress.mean_name_bytes).saturating_mul(progress.retained_nodes),
            Ordering::Relaxed,
        );

        counters.set_state(progress.state);
        counters.offer_current_display(progress.current_dir.clone());
    }
}

/// Mirrors every recorded failure into the shell's own bounded error log.
///
/// The scanner keeps the authoritative list in
/// [`CompletedScan::errors`](rdirstat_core::CompletedScan::errors), but that
/// only exists once the scan is over — and "what are these 334 errors" is a
/// question asked *while* the counter is climbing. This is the live half:
/// per-class counts and the first few failures in full, read by `scan_errors`.
struct ErrorRecorder {
    counters: Arc<ProgressCounters>,
}

impl ErrorSink for ErrorRecorder {
    fn record(&self, error: &ScanError) {
        self.counters.record_error(error);
    }
}

// ---------------------------------------------------------------------------
// Categorization
// ---------------------------------------------------------------------------

/// Joins the scanner's categorizer seam to the classifier's taxonomy.
///
/// `rdirstat_scan::Categorizer` passes the entry's scan-time flags;
/// `rdirstat_classify::Categorizer` wants `st_mode`, from which it reads only
/// the execute bits. The scanner has already resolved "is this executable" into
/// [`flags::EXECUTABLE`], so the adapter re-expresses that one bit rather than
/// making either crate carry the other's argument.
#[derive(Debug)]
struct ClassifyAdapter {
    inner: &'static rdirstat_classify::Categorizer,
}

impl ScanCategorizer for ClassifyAdapter {
    fn categorize(&self, name: &[u8], kind: Kind, flags_bits: u16) -> CategoryId {
        let mode = if flags_bits & flags::EXECUTABLE == 0 {
            0
        } else {
            rdirstat_classify::MODE_EXECUTABLE_BITS
        };
        self.inner.classify(name, kind, mode)
    }
}

/// The compiled default taxonomy, or `None` if it does not compile.
///
/// A taxonomy that fails to compile must not fail the scan: the tree, the
/// sizes, and every action are correct without it, and colouring is the only
/// thing lost. The failure is logged rather than swallowed silently.
fn shared_categorizer() -> Option<(Arc<dyn ScanCategorizer>, ConfigHash)> {
    match rdirstat_classify::Categorizer::shared() {
        Ok(inner) => {
            let hash = inner.config_hash();
            Some((Arc::new(ClassifyAdapter { inner }), hash))
        }
        Err(error) => {
            tracing::error!(%error, "the default category table did not compile; scanning uncategorized");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Validates options before a scan slot is claimed.
///
/// Runs the scanner's own validation *and* compiles the exclusion rules, so a
/// bad pattern or an unsupported syntax is rejected at `scan_start` with a
/// typed error instead of failing a scan the user already watched begin.
///
/// # Errors
///
/// [`StartError::InvalidOptions`] naming the offending option or pattern.
pub(crate) fn validate(options: &ScanOptions) -> Result<(), StartError> {
    rdirstat_scan::validate_options(options)?;
    // `ExclusionSet::compile` is the single validation point for patterns; it
    // takes the rules by value, hence the clone. It is bounded by the user's
    // rule count, and happens once per scan.
    let rules: Vec<ExclusionRule> = options.exclusions.clone();
    rdirstat_scan::ExclusionSet::compile(rules).map(|_| ())
}

/// Runs one scan to completion, cancellation, or fatal failure.
///
/// **Blocks.** The caller runs it on a dedicated thread.
pub(crate) fn run(request: ScanRequest) -> ScanOutcome {
    let ScanRequest {
        root,
        options,
        scan_id,
        generation,
        cancel,
        counters,
    } = request;

    counters.set_state(ScanState::Scanning);

    let engine = rdirstat_scan::engine_for(&options);
    let mut scanner = Scanner::new()
        .with_options(options)
        .with_engine(engine)
        .with_cancel(cancel.scan_token())
        .with_progress(Arc::new(CounterSink {
            counters: Arc::clone(&counters),
        }))
        .with_error_sink(Arc::new(ErrorRecorder {
            counters: Arc::clone(&counters),
        }))
        .with_scan_id(scan_id)
        .with_generation(generation);

    if let Some((categorizer, hash)) = shared_categorizer() {
        scanner = scanner.with_categorizer(categorizer, hash);
    }

    match scanner.scan(&root) {
        Ok(rdirstat_scan::ScanOutcome::Completed(scan)) => {
            counters.set_state(ScanState::Ready);
            ScanOutcome::Completed(scan)
        }
        Ok(rdirstat_scan::ScanOutcome::Cancelled) => {
            cancel.mark_resources_closed();
            counters.set_state(ScanState::Cancelling);
            ScanOutcome::Cancelled
        }
        // `scan_start` already ran `validate`, so a `Start` failure here is
        // something that changed between validation and the scan — the root
        // vanished, or its permissions did. It is reported as the fatal scan
        // error it is rather than being silently downgraded.
        Err(rdirstat_scan::ScanFailure::Start(error)) => {
            counters.set_state(ScanState::Failed);
            ScanOutcome::Failed(ScanError::RootUnavailable {
                path: DisplayPath::from_bytes(path_bytes(&root)),
                reason: error.to_string(),
                class: rdirstat_core::ErrorClass::Other,
            })
        }
        Err(rdirstat_scan::ScanFailure::Scan(error)) => {
            counters.set_state(ScanState::Failed);
            ScanOutcome::Failed(error)
        }
        // `ScanOutcome` and `ScanFailure` are both `#[non_exhaustive]`: a
        // variant added upstream must not silently become "completed".
        Ok(other) => {
            counters.set_state(ScanState::Failed);
            ScanOutcome::Failed(ScanError::RootUnavailable {
                path: DisplayPath::from_bytes(path_bytes(&root)),
                reason: format!("unrecognized scan outcome: {other:?}"),
                class: rdirstat_core::ErrorClass::Other,
            })
        }
        Err(other) => {
            counters.set_state(ScanState::Failed);
            ScanOutcome::Failed(ScanError::RootUnavailable {
                path: DisplayPath::from_bytes(path_bytes(&root)),
                reason: format!("unrecognized scan failure: {other:?}"),
                class: rdirstat_core::ErrorClass::Other,
            })
        }
    }
}

/// A path as filesystem bytes. Paths are not strings (docs/08).
fn path_bytes(path: &std::path::Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdirstat_core::{RuleAction, RuleScope, RuleSyntax};

    fn options() -> ScanOptions {
        ScanOptions::default()
    }

    /// Two scans running at once, through the real engine, publishing two
    /// real trees into one `AppState`.
    ///
    /// Everything in `state.rs` drives the admission machine with fixture
    /// device numbers and never starts a thread. That leaves the join between
    /// the two halves — admit, run, publish, query both — untested, and it is
    /// exactly the join the feature is. This test walks it end to end with the
    /// scanner actually reading directories.
    ///
    /// The two roots are given **different device numbers** so admission
    /// permits them concurrently; a fixture cannot conjure a second physical
    /// disk, and what is under test here is the concurrency of the engine and
    /// the plurality of the published map, not `st_dev` itself.
    #[test]
    fn two_scans_run_at_once_and_both_trees_are_queryable() {
        use crate::state::AppState;
        use std::sync::Arc as StdArc;

        let left = tempfile::tempdir().expect("tempdir");
        let right = tempfile::tempdir().expect("tempdir");
        for (dir, count) in [(&left, 8_usize), (&right, 12_usize)] {
            for index in 0..count {
                std::fs::write(dir.path().join(format!("f{index}.bin")), vec![b'x'; 64]).expect("write");
            }
            std::fs::create_dir(dir.path().join("nested")).expect("mkdir");
            std::fs::write(dir.path().join("nested/deep.bin"), b"deep").expect("write");
        }

        let state = StdArc::new(AppState::new());

        // Distinct devices, so admission starts both rather than queueing one.
        let first = state
            .accept(left.path().to_path_buf(), display_of(left.path()), 101, options())
            .expect("accepted")
            .started("the first scan");
        let second = state
            .accept(right.path().to_path_buf(), display_of(right.path()), 202, options())
            .expect("accepted")
            .started("the second scan");

        assert_eq!(state.status().running.len(), 2, "both scans are admitted");

        // Genuinely simultaneous: both threads are spawned before either is
        // joined, so the engines overlap rather than running back to back.
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|pending| {
                let state = StdArc::clone(&state);
                std::thread::spawn(move || {
                    let outcome = run(ScanRequest {
                        root: pending.root.clone(),
                        options: pending.options.clone(),
                        scan_id: pending.scan_id,
                        generation: pending.generation,
                        cancel: StdArc::clone(&pending.cancel),
                        counters: StdArc::clone(&pending.counters),
                    });
                    match outcome {
                        ScanOutcome::Completed(scan) => {
                            state.publish(StdArc::from(scan));
                            pending.generation
                        }
                        other => panic!("a fixture scan must complete, got {other:?}"),
                    }
                })
            })
            .collect();

        let generations: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("the scan thread must not panic"))
            .collect();
        assert_eq!(generations.len(), 2);

        // The property that matters: BOTH trees are addressable afterwards.
        // Under the old single-slot machine the second publish replaced the
        // first, and this assertion is what would have caught that.
        for generation in &generations {
            let tree = state
                .tree_for_query(*generation)
                .unwrap_or_else(|error| panic!("generation {generation:?} must be resident: {error:?}"));
            assert_eq!(tree.generation, *generation);
            assert!(tree.tree.len() > 1, "a scanned tree has nodes");
            assert!(tree.tree.retained_bytes() > 0, "and reports its memory");
        }

        let status = state.status();
        assert!(status.running.is_empty(), "both scans retired");
        assert_eq!(status.ready.len(), 2, "and both trees are offered");
        // Different roots, so the two must not be the same tree twice.
        assert_ne!(status.ready[0].root, status.ready[1].root);
    }

    /// Two scans on the SAME device: the second waits, then runs when the
    /// first ends, and its tree joins the first rather than replacing it.
    #[test]
    fn a_same_device_scan_waits_then_runs_and_both_trees_survive() {
        use crate::state::AppState;
        use std::sync::Arc as StdArc;

        let dir = tempfile::tempdir().expect("tempdir");
        let left = dir.path().join("left");
        let right = dir.path().join("right");
        for path in [&left, &right] {
            std::fs::create_dir(path).expect("mkdir");
            std::fs::write(path.join("a.bin"), b"hello").expect("write");
        }

        let state = AppState::new();
        // One device for both, which is the real shape: two folders on one disk.
        let first = state
            .accept(left.clone(), display_of(&left), 7, options())
            .expect("accepted")
            .started("the first scan");
        let waited = state
            .accept(right.clone(), display_of(&right), 7, options())
            .expect("accepted");
        assert!(
            matches!(waited, crate::state::Admission::Wait { .. }),
            "a same-device scan must wait, got {waited:?}"
        );

        let ScanOutcome::Completed(scan) = run(ScanRequest {
            root: first.root.clone(),
            options: first.options.clone(),
            scan_id: first.scan_id,
            generation: first.generation,
            cancel: StdArc::clone(&first.cancel),
            counters: StdArc::clone(&first.counters),
        }) else {
            panic!("the first scan must complete");
        };
        state.publish(StdArc::from(scan));

        // The device is free, so the queued scan is released.
        let released = state.take_admissible();
        assert_eq!(released.len(), 1, "the waiting scan is released");
        let second = &released[0];

        let ScanOutcome::Completed(scan) = run(ScanRequest {
            root: second.root.clone(),
            options: second.options.clone(),
            scan_id: second.scan_id,
            generation: second.generation,
            cancel: StdArc::clone(&second.cancel),
            counters: StdArc::clone(&second.counters),
        }) else {
            panic!("the second scan must complete");
        };
        state.publish(StdArc::from(scan));

        assert!(
            state.tree_for_query(first.generation).is_ok(),
            "the first tree survived"
        );
        assert!(state.tree_for_query(second.generation).is_ok(), "and so did the second");
        assert_eq!(state.status().ready.len(), 2);
    }

    fn display_of(path: &std::path::Path) -> rdirstat_core::DisplayPath {
        rdirstat_core::DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes())
    }

    /// The live error log is fed by the scan, not by the completed result.
    ///
    /// This is the whole point of the `ErrorSink` seam: a user watching "334
    /// errors" climb wants to know what they are *now*, and `CompletedScan`
    /// does not exist yet. A directory the process cannot open is the cheapest
    /// reproduction of a real permission denial, which is what a scan of `/`
    /// hits by the thousand.
    #[test]
    fn a_running_scan_records_what_failed_and_not_only_how_many() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let readable = dir.path().join("readable");
        let refused = dir.path().join("refused");
        std::fs::create_dir(&readable).expect("mkdir");
        std::fs::write(readable.join("a.txt"), b"x").expect("write");
        std::fs::create_dir(&refused).expect("mkdir");
        std::fs::write(refused.join("hidden.txt"), b"x").expect("write");
        std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        // Running as root would read it anyway and the test would prove nothing.
        if std::fs::read_dir(&refused).is_ok() {
            std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o755)).ok();
            return;
        }

        let counters = Arc::new(ProgressCounters::new());
        let outcome = run(ScanRequest {
            root: dir.path().to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::clone(&counters),
        });
        // Restore before asserting, so a failure does not leave an undeletable
        // directory behind for TempDir::drop.
        std::fs::set_permissions(&refused, std::fs::Permissions::from_mode(0o755)).ok();
        assert!(
            matches!(outcome, ScanOutcome::Completed(_)),
            "a refused child is not fatal"
        );

        let counts = counters.error_counts();
        let denied: u64 = counts
            .iter()
            .filter(|entry| entry.class == rdirstat_core::ErrorClass::PermissionDenied)
            .map(|entry| entry.count)
            .sum();
        assert_eq!(denied, 1, "one refused directory, counted once: {counts:?}");

        let samples = counters.error_samples(16);
        assert_eq!(samples.len(), 1, "and kept in full, with its path");
        let text = samples[0].to_string();
        assert!(
            text.contains("refused"),
            "the sample names the path that failed: {text}"
        );
    }

    #[test]
    fn a_regex_exclusion_is_refused_before_a_scan_slot_is_claimed() {
        let mut opts = options();
        opts.exclusions = vec![ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::RootRelativePath,
            syntax: RuleSyntax::Regex,
            pattern: "^/tmp/.*$".to_owned(),
            case_sensitive: true,
        }];
        let error = validate(&opts).expect_err("a regex rule is not supported by this build");
        assert!(matches!(error, StartError::InvalidOptions { .. }));
    }

    #[test]
    fn an_empty_exclusion_pattern_is_refused() {
        let mut opts = options();
        opts.exclusions = vec![ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::DirectoryName,
            syntax: RuleSyntax::Glob,
            pattern: String::new(),
            case_sensitive: false,
        }];
        assert!(validate(&opts).is_err(), "an empty pattern matches everything");
    }

    #[test]
    fn zero_workers_is_refused() {
        let mut opts = options();
        opts.workers = Some(0);
        assert!(validate(&opts).is_err());
    }

    #[test]
    fn the_default_options_validate() {
        validate(&options()).expect("the shipped defaults are startable");
    }

    #[test]
    fn aggregation_is_refused_rather_than_silently_ignored() {
        // The old in-tree scanner implemented aggregation; rdirstat-scan does
        // not, and refuses instead of returning a full-detail scan that claims
        // to be aggregated. This test exists so the day it *is* implemented,
        // someone has to come here and delete it deliberately.
        let mut opts = options();
        opts.aggregate_below_bytes = Some(4_096);
        assert!(validate(&opts).is_err());
    }

    #[test]
    fn the_default_taxonomy_compiles_and_categorizes() {
        let (categorizer, hash) = shared_categorizer().expect("the shipped category table compiles");
        assert!(!hash.as_str().is_empty(), "a compiled table has a config hash");

        let jpeg = categorizer.categorize(b"holiday.jpg", Kind::File, flags::NONE);
        assert_ne!(
            jpeg,
            CategoryId::UNCATEGORIZED,
            "a .jpg must not come back uncategorized; that was the pre-swap bug"
        );

        // The execute bit is the one thing the adapter has to re-express.
        let plain = categorizer.categorize(b"run-me", Kind::File, flags::NONE);
        let exec = categorizer.categorize(b"run-me", Kind::File, flags::EXECUTABLE);
        assert_ne!(plain, exec, "the EXECUTABLE flag must reach the classifier as st_mode");
    }

    #[test]
    fn cancelling_the_shell_token_cancels_the_scanner_token() {
        let token = CancelToken::new();
        let scan_token = token.scan_token();
        assert!(!scan_token.is_cancelled());
        token.request();
        assert!(token.is_cancelled(), "the shell's own flag is set");
        assert!(
            scan_token.is_cancelled(),
            "and the scanner sees it with no bridge thread"
        );
    }
}
