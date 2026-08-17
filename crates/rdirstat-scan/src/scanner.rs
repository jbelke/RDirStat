//! The public entry point: options in, one [`CompletedScan`] out.
//!
//! A partial-failure scan is a **success with a payload**, never a
//! `Result::Err`: `EACCES` on one directory marks a node unreadable and the
//! scan continues (docs/08-RUST-PRACTICES.md#errors-thiserror-in-libraries-anyhow-in-binaries).
//! Only three things end a scan — a root that stopped being the root, the
//! memory ceiling, and an exhausted arena.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rdirstat_core::{
    CompletedScan, ConfigHash, DisplayPath, ErrorClass, MAX_TREE_DEPTH, ScanError, ScanId, ScanOptions, ScanState,
    StartError, TreeGeneration, VolumeId,
};

use crate::builder::{BuilderConfig, ScanBuilder, path_bytes};
use crate::cancel::CancelToken;
use crate::categorize::{Categorizer, Uncategorized};
use crate::engine::{Completion, Engine, EngineContext, parallel, sequential};
use crate::exclude::{ExclusionSet, default_exclusions};
use crate::progress::{Counters, CurrentDir, ErrorSink, NoErrors, NoProgress, ProgressPublisher, ProgressSink};
use crate::reader::{DirReader, classify_os_error};
use crate::std_reader::StdReader;

/// How a scan ended.
///
/// A cancelled scan never becomes a [`CompletedScan`]: it is not published, not
/// saved, and not catalogued.
#[derive(Debug)]
#[non_exhaustive]
pub enum ScanOutcome {
    /// The traversal finished and the tree was frozen.
    Completed(Box<CompletedScan>),
    /// Cancellation was observed; partial state was discarded.
    Cancelled,
}

impl ScanOutcome {
    /// The completed scan, or `None` if it was cancelled.
    #[must_use]
    pub fn completed(self) -> Option<Box<CompletedScan>> {
        match self {
            Self::Completed(scan) => Some(scan),
            Self::Cancelled => None,
        }
    }

    /// Whether the scan was cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// A scan could not start, or ended on a fatal error.
///
/// Split deliberately: `src-tauri`'s `scan_start` returns
/// `Result<ScanId, StartError>` before any traversal happens, and everything
/// after that arrives as an event.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanFailure {
    /// Rejected before traversal: a bad root or bad options.
    #[error(transparent)]
    Start(#[from] StartError),
    /// A fatal failure during traversal.
    #[error(transparent)]
    Scan(#[from] ScanError),
}

/// Configures and runs one scan.
///
/// Every collaborator is injectable so the differential tests can hold the
/// reader constant and vary only the scheduler.
#[derive(Clone)]
pub struct Scanner {
    options: ScanOptions,
    engine: Engine,
    cancel: CancelToken,
    scan_id: ScanId,
    generation: TreeGeneration,
    tool_version: String,
    reader: Arc<dyn DirReader>,
    categorizer: Arc<dyn Categorizer>,
    progress: Arc<dyn ProgressSink>,
    errors: Arc<dyn ErrorSink>,
    category_config_hash: ConfigHash,
}

impl core::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Scanner")
            .field("engine", &self.engine)
            .field("reader", &self.reader.name())
            .field("scan_id", &self.scan_id)
            .finish_non_exhaustive()
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    /// A scanner with default options, the [`StdReader`], no categories, and no
    /// progress sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            options: ScanOptions::default(),
            engine: Engine::default(),
            cancel: CancelToken::new(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            tool_version: env!("CARGO_PKG_VERSION").to_owned(),
            reader: Arc::new(StdReader::new()),
            categorizer: Arc::new(Uncategorized),
            progress: Arc::new(NoProgress),
            errors: Arc::new(NoErrors),
            category_config_hash: ConfigHash::default(),
        }
    }

    /// Replaces the options. The worker count in the options is advisory; the
    /// engine decides, and the result records what actually ran.
    #[must_use]
    pub fn with_options(mut self, options: ScanOptions) -> Self {
        self.options = options;
        self
    }

    /// Chooses the scheduler.
    #[must_use]
    pub const fn with_engine(mut self, engine: Engine) -> Self {
        self.engine = engine;
        self
    }

    /// Uses a caller-owned cancellation token, so the caller can cancel.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Replaces the directory reader.
    #[must_use]
    pub fn with_reader(mut self, reader: Arc<dyn DirReader>) -> Self {
        self.reader = reader;
        self
    }

    /// Wires the categorizer. Without one every node is
    /// [`CategoryId::UNCATEGORIZED`](rdirstat_core::CategoryId::UNCATEGORIZED).
    #[must_use]
    pub fn with_categorizer(mut self, categorizer: Arc<dyn Categorizer>, config_hash: ConfigHash) -> Self {
        self.categorizer = categorizer;
        self.category_config_hash = config_hash;
        self
    }

    /// Wires the progress sink.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Self {
        self.progress = progress;
        self
    }

    /// Wires the error sink, which observes every recorded failure as it
    /// happens rather than waiting for [`CompletedScan::errors`] at the end.
    ///
    /// [`CompletedScan::errors`]: rdirstat_core::CompletedScan::errors
    #[must_use]
    pub fn with_error_sink(mut self, errors: Arc<dyn ErrorSink>) -> Self {
        self.errors = errors;
        self
    }

    /// Sets the scan id carried in every progress event.
    #[must_use]
    pub const fn with_scan_id(mut self, scan_id: ScanId) -> Self {
        self.scan_id = scan_id;
        self
    }

    /// Sets the generation the result will be published under.
    #[must_use]
    pub const fn with_generation(mut self, generation: TreeGeneration) -> Self {
        self.generation = generation;
        self
    }

    /// The cancellation token, for a caller that did not supply one.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Runs the scan.
    ///
    /// # Errors
    ///
    /// [`ScanFailure::Start`] before any traversal — a missing root, a root
    /// that is not a directory, a refused root, or options this build cannot
    /// honour. [`ScanFailure::Scan`] for the three fatal in-scan failures.
    /// Everything else is recorded in [`CompletedScan::errors`] and the scan
    /// still succeeds.
    pub fn scan(&self, root: &Path) -> Result<ScanOutcome, ScanFailure> {
        validate_options(&self.options)?;
        let root = resolve_root(root)?;
        let metadata = fs::metadata(&root).map_err(|error| start_error(&root, &error))?;
        if !metadata.is_dir() {
            return Err(StartError::RootNotADirectory {
                path: DisplayPath::from_bytes(path_bytes(&root)),
            }
            .into());
        }
        // Opening it once up front turns "no Full Disk Access" into a start
        // failure the UI can act on, instead of an empty tree.
        drop(fs::read_dir(&root).map_err(|error| start_error(&root, &error))?);

        let root_device = metadata.dev();
        let volume = volume_id(&root, root_device);

        let mut rules = Vec::new();
        if self.options.apply_default_exclusions {
            // Prepended, per `ScanOptions::apply_default_exclusions`.
            rules.extend(default_exclusions(&root));
        }
        rules.extend(self.options.exclusions.iter().cloned());
        let exclusions = ExclusionSet::compile(rules)?;
        let exclusion_hash = ConfigHash::from_digest(&digest32(&exclusions.canonical_text()));

        let counters = Counters::new();
        let current = CurrentDir::new();
        let started_wall = Instant::now();
        let started_unix_ms = unix_ms_now();
        let mut publisher = ProgressPublisher::new(self.progress.as_ref(), self.scan_id, started_wall);

        let mut builder = ScanBuilder::new(
            &root,
            BuilderConfig {
                exclusions: &exclusions,
                categorizer: self.categorizer.as_ref(),
                counters: &counters,
                error_sink: self.errors.as_ref(),
                cross_filesystems: self.options.cross_filesystems,
                count_hard_links_once: self.options.count_hard_links_once,
                root_device,
                root_mtime: metadata.mtime(),
            },
        )?;
        let root_handle = builder.root_handle(&root);

        let context = EngineContext {
            reader: self.reader.as_ref(),
            cancel: &self.cancel,
            counters: &counters,
            current: &current,
            memory_limit: self.options.memory_limit_bytes,
        };

        publisher.publish(ScanState::Scanning, &counters, &current, builder.footprint(1));
        let completion = match self.engine {
            Engine::Sequential => sequential::run(&mut builder, root_handle, &context, &mut publisher)?,
            Engine::Parallel { workers } => parallel::run(
                &mut builder,
                root_handle,
                &context,
                &mut publisher,
                usize::from(workers.get()),
            )?,
        };

        if completion == Completion::Cancelled {
            publisher.publish(ScanState::Cancelling, &counters, &current, builder.footprint(0));
            drop(builder);
            return Ok(ScanOutcome::Cancelled);
        }

        publisher.publish(ScanState::Finalizing, &counters, &current, builder.footprint(0));
        let artifacts = builder.finish()?;
        let finished_unix_ms = unix_ms_now();

        let mut options = self.options.clone();
        options.workers = Some(self.engine.workers());
        options.exclusions = exclusions.rules().to_vec();

        let scan = CompletedScan {
            scan_id: self.scan_id,
            generation: self.generation,
            root_path: root,
            root: artifacts.tree.root(),
            volume,
            started_unix_ms,
            finished_unix_ms,
            options,
            exclusion_hash,
            category_config_hash: self.category_config_hash.clone(),
            tool_version: self.tool_version.clone(),
            counts: artifacts.counts,
            totals: artifacts.totals,
            mutations: artifacts.mutations,
            errors: artifacts.errors,
            error_counts: artifacts.error_counts,
            excluded_roots: artifacts.excluded_roots,
            tree: artifacts.tree,
        };
        publisher.publish(
            ScanState::Ready,
            &counters,
            &current,
            crate::progress::ArenaFootprint::measure(scan.tree.len(), 0, 0, scan.tree.directory_count(), 0),
        );
        Ok(ScanOutcome::Completed(Box::new(scan)))
    }
}

/// Rejects options this build cannot honour, instead of honouring them
/// silently and reporting a number that is not what was asked for.
///
/// # Errors
///
/// [`StartError::InvalidOptions`].
pub fn validate_options(options: &ScanOptions) -> Result<(), StartError> {
    if options.workers == Some(0) {
        return Err(StartError::InvalidOptions {
            detail: "worker count must be at least 1".to_owned(),
        });
    }
    if options.aggregate_below_bytes.is_some() {
        return Err(StartError::InvalidOptions {
            detail: "aggregate_below_bytes is not implemented in this build; scan at full detail".to_owned(),
        });
    }
    Ok(())
}

/// The engine implied by `options`, for a caller that configures workers there.
#[must_use]
pub fn engine_for(options: &ScanOptions) -> Engine {
    match options.workers {
        Some(1) => Engine::parallel(1),
        Some(workers) => Engine::parallel(workers),
        None => Engine::default(),
    }
}

fn resolve_root(root: &Path) -> Result<PathBuf, StartError> {
    match fs::canonicalize(root) {
        Ok(resolved) => Ok(resolved),
        Err(error) => Err(start_error(root, &error)),
    }
}

fn start_error(root: &Path, error: &io::Error) -> StartError {
    let path = DisplayPath::from_bytes(path_bytes(root));
    match classify_os_error(error) {
        ErrorClass::NotFound => StartError::RootNotFound { path },
        ErrorClass::NotADirectory => StartError::RootNotADirectory { path },
        ErrorClass::PermissionDenied => StartError::PermissionDenied {
            path,
            os_code: error.raw_os_error().unwrap_or(0),
        },
        _ => StartError::Internal(format!("{}: {error}", root.display())),
    }
}

/// Builds the volume record.
///
/// `fs_type` and `volume_uuid` need `statfs(2)`/`getattrlist(2)`, which need
/// `unsafe`; this crate has none, so they are reported as unknown rather than
/// guessed. The device number, the mount point, and the case behaviour are all
/// derived from `stat` alone.
fn volume_id(root: &Path, device: u64) -> VolumeId {
    let mount_point = find_mount_point(root, device);
    let case_sensitive = probe_case_sensitivity(root).unwrap_or(false);
    VolumeId {
        device,
        fs_type: "unknown".to_owned(),
        volume_uuid: None,
        mount_point: DisplayPath::from_bytes(path_bytes(&mount_point)),
        // Every filesystem macOS ships preserves case; only lookup differs.
        case_preserving: true,
        case_sensitive,
    }
}

/// Walks up until the device number changes. Bounded, so a pathological path
/// cannot loop.
fn find_mount_point(root: &Path, device: u64) -> PathBuf {
    let mut current = root.to_path_buf();
    let mut steps = 0_u32;
    while steps < MAX_TREE_DEPTH {
        let Some(parent) = current.parent().map(Path::to_path_buf) else {
            break;
        };
        let Ok(metadata) = fs::symlink_metadata(&parent) else {
            break;
        };
        if metadata.dev() != device {
            break;
        }
        current = parent;
        steps += 1;
    }
    current
}

/// Read-only probe: take an existing entry with an ASCII letter, flip its case,
/// and see whether the flipped name resolves to the same inode.
///
/// Never writes. APFS can be configured either way and the answer must not be
/// assumed.
fn probe_case_sensitivity(root: &Path) -> Option<bool> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.take(256) {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(name.as_os_str());
        if !bytes.iter().any(u8::is_ascii_alphabetic) {
            continue;
        }
        let flipped: Vec<u8> = bytes
            .iter()
            .map(|byte| {
                if byte.is_ascii_lowercase() {
                    byte.to_ascii_uppercase()
                } else {
                    byte.to_ascii_lowercase()
                }
            })
            .collect();
        let original = fs::symlink_metadata(entry.path()).ok()?;
        let flipped_path = crate::builder::join_bytes(root, &flipped);
        return Some(match fs::symlink_metadata(&flipped_path) {
            // The flipped name resolves to the same object: lookup ignores case.
            Ok(other) => !(other.dev() == original.dev() && other.ino() == original.ino()),
            Err(_) => true,
        });
    }
    None
}

/// A 256-bit configuration fingerprint.
///
/// **Not cryptographic.** Four FNV-1a-64 lanes with distinct seeds. It answers
/// "were these two scans configured the same way?", which is the only question
/// [`ConfigHash`] is asked; nothing in this product treats it as a security
/// boundary. Swapping in SHA-256 later changes only this function.
fn digest32(text: &str) -> [u8; 32] {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut out = [0_u8; 32];
    for (lane, chunk) in out.chunks_mut(8).enumerate() {
        let mut hash = OFFSET ^ (u64::try_from(lane).unwrap_or(0)).wrapping_mul(PRIME);
        for byte in text.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        // Fold the length in so a prefix cannot collide with the whole.
        hash ^= u64::try_from(text.len()).unwrap_or(u64::MAX);
        hash = hash.wrapping_mul(PRIME);
        chunk.copy_from_slice(&hash.to_be_bytes());
    }
    out
}

fn unix_ms_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis())
            .unwrap_or(i64::MAX)
            .saturating_neg(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregation_and_zero_workers_are_refused_not_ignored() {
        let mut options = ScanOptions::default();
        options.aggregate_below_bytes = Some(4_096);
        assert!(matches!(
            validate_options(&options),
            Err(StartError::InvalidOptions { .. })
        ));
        options.aggregate_below_bytes = None;
        options.workers = Some(0);
        assert!(matches!(
            validate_options(&options),
            Err(StartError::InvalidOptions { .. })
        ));
        options.workers = Some(3);
        assert!(validate_options(&options).is_ok());
        assert_eq!(engine_for(&options).workers(), 3);
        assert_eq!(engine_for(&ScanOptions::default()).workers(), Engine::DEFAULT_WORKERS);
    }

    #[test]
    fn the_config_digest_is_stable_and_sensitive() {
        let a = digest32("exclude\tpath\tglob\tcs\tVolumes/*\n");
        let b = digest32("exclude\tpath\tglob\tcs\tVolumes/*\n");
        let c = digest32("exclude\tpath\tglob\tcs\tVolumes/x\n");
        assert_eq!(a, b, "the same rule set hashes the same way twice");
        assert_ne!(a, c);
        assert_eq!(ConfigHash::from_digest(&a).as_str().len(), 64);
        assert_ne!(digest32(""), digest32(" "));
    }

    #[test]
    fn a_missing_root_is_a_start_error_not_a_scan_error() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing = temp.path().join("nope");
        let error = Scanner::new().scan(&missing).expect_err("missing root");
        assert!(matches!(error, ScanFailure::Start(StartError::RootNotFound { .. })));
    }

    #[test]
    fn a_file_root_is_a_start_error() {
        let temp = tempfile::tempdir().expect("temp dir");
        let file = temp.path().join("f");
        fs::write(&file, b"x").expect("write");
        let error = Scanner::new().scan(&file).expect_err("not a directory");
        assert!(matches!(
            error,
            ScanFailure::Start(StartError::RootNotADirectory { .. })
        ));
    }

    #[test]
    fn the_mount_point_walk_terminates_at_the_root_of_the_filesystem() {
        let temp = tempfile::tempdir().expect("temp dir");
        let device = fs::symlink_metadata(temp.path()).expect("stat").dev();
        let mount = find_mount_point(temp.path(), device);
        assert!(temp.path().starts_with(&mount), "{mount:?} should contain the fixture");
    }
}
