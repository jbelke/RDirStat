//! Copy what one directory has and another is missing.
//!
//! The user's framing: pick a source and a destination, copy over the missing
//! files. That is a narrower and much safer operation than a general two-way
//! sync, and the narrowness is the point:
//!
//! **Nothing in the destination is ever deleted or overwritten by default.**
//! This is additive. A file that exists on both sides is left alone unless the
//! user explicitly asks for differing files to be replaced, and a file that
//! exists only in the destination is never touched at all. "Sync" in the
//! rsync `--delete` sense — making the destination mirror the source — is not
//! what was asked for and is the version that eats data, so it is not here.
//!
//! ## How "missing" is decided
//!
//! Three tiers, cheapest first, because the expensive one is genuinely
//! expensive on a large tree:
//!
//! 1. **Not present** at the same relative path. Copy it.
//! 2. **Present, different size.** Different file. Copy it if replacing is on.
//! 3. **Present, same size.** Under [`CompareMode::Quick`] that is treated as
//!    the same file. Under [`CompareMode::Verify`] the bytes are compared, so
//!    a file that was corrupted or edited-in-place without changing length is
//!    caught. Verify reads both sides in full, so it costs roughly the size of
//!    the overlap — worth offering, wrong as a default.
//!
//! Deliberately NOT using mtime as evidence of sameness. Copy tools routinely
//! rewrite it, archives restore it, and a same-size-same-mtime file that
//! differs is exactly the case a user reaches for a sync tool to find.
//!
//! ## Why `ditto`
//!
//! Same reason as [`crate::relocate`]: it is Apple's own copier and the only
//! one that reliably carries ACLs, extended attributes, resource forks and
//! compression. Unlike relocate, a destination that cannot hold that metadata
//! is a WARNING here rather than a refusal — relocate deletes the source, so
//! metadata loss there is permanent, whereas a sync leaves the original in
//! place and the user can copy it again somewhere better.

use std::collections::BTreeMap;
use std::fs;
use std::hash::BuildHasher;
use std::io::{self, Read as _};
use std::path::{Component, Path};
use std::process::Command;

use rdirstat_core::{ConfirmationToken, DisplayPath, TreeGeneration};
use serde::{Deserialize, Serialize};

use crate::token::{self, ItemIdentity};

/// Chunk size for the content comparison.
const COMPARE_CHUNK: usize = 256 * 1024;

/// Ceiling on entries a plan will enumerate.
///
/// A plan is rendered as a list the user reviews before confirming, and a
/// list of a million rows is not a review. Past this the plan reports the true
/// count and says it truncated the listing, rather than pretending the tail
/// does not exist or silently refusing to sync it.
const MAX_PLANNED_ENTRIES: usize = 5_000;

/// Directories never descended into on either side.
///
/// These are macOS bookkeeping, not user data: copying them produces errors at
/// best and a corrupt-looking destination at worst.
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[".Spotlight-V100", ".fseventsd", ".TemporaryItems", ".Trashes"];

/// How hard to look before calling two same-sized files the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompareMode {
    /// Path and size only. Fast enough to plan a large tree interactively.
    Quick,
    /// Also compares contents when sizes match. Reads both sides in full.
    Verify,
}

/// What to do about a file that exists on both sides but differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OnDiffer {
    /// Leave it. The destination keeps whatever it has. The default.
    Skip,
    /// Overwrite it from the source. The only setting that destroys anything.
    Replace,
}

/// Why one file is in the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncReason {
    /// No file at that relative path in the destination.
    Missing,
    /// Present, but a different size.
    SizeDiffers,
    /// Present and the same size, but the bytes differ. Only ever produced
    /// under [`CompareMode::Verify`].
    ContentDiffers,
}

/// One file the sync would copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncEntry {
    /// Path relative to the source root, which is also its path under the
    /// destination root. Shown to the user; never used as authority.
    pub relative_path: String,
    pub bytes: u64,
    pub reason: SyncReason,
}

/// Something the user should read before confirming.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncWarning {
    pub code: String,
    pub message: String,
}

impl SyncWarning {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_owned(), message: message.into() }
    }
}

/// What a sync would do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncPlan {
    pub generation: TreeGeneration,
    /// `None` when the sync cannot proceed. The UI keys its button on this.
    pub token: Option<ConfirmationToken>,
    pub source: DisplayPath,
    pub destination: DisplayPath,
    pub compare_mode: CompareMode,
    pub on_differ: OnDiffer,
    /// The files that would be copied, capped at [`MAX_PLANNED_ENTRIES`].
    pub entries: Vec<SyncEntry>,
    /// True count of files to copy, even when `entries` was truncated.
    pub total_to_copy: u64,
    pub bytes_to_copy: u64,
    /// Files present and identical on both sides. Nothing to do for these.
    pub already_present: u64,
    /// Files that differ and are being left alone because `on_differ` is Skip.
    pub differing_skipped: u64,
    /// Sockets, FIFOs and device nodes seen in the source. Nothing can copy
    /// them, so they are counted and excluded rather than failing the sync.
    pub special_skipped: u64,
    /// Directories that could not be read on either side. Their contents are
    /// not in the plan, so the plan is a floor.
    pub unreadable: u64,
    pub destination_available: u64,
    pub destination_filesystem: String,
    pub entries_truncated: bool,
    pub warnings: Vec<SyncWarning>,
}

/// What a sync actually did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncReport {
    pub generation: TreeGeneration,
    pub source: DisplayPath,
    pub destination: DisplayPath,
    pub copied: u64,
    pub bytes_copied: u64,
    /// Per-file failures. A sync continues past one bad file.
    pub failures: Vec<SyncFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncFailure {
    pub relative_path: String,
    pub reason: String,
}

/// A sync-specific failure.
///
/// `Display` and `Error` are hand-written for the same reason as
/// [`crate::relocate::RelocateError`]: `src-tauri` does not depend on
/// `thiserror`, and adding it for one enum is the wrong end of that trade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub(crate) enum SyncError {
    /// A path was relative, contained `..`, or was not a directory.
    BadPath { path: DisplayPath, reason: String },
    /// Source and destination are the same tree, or one contains the other.
    Overlapping { source: DisplayPath, destination: DisplayPath },
    /// The destination does not have room for what would be copied.
    NotEnoughSpace { needed: u64, available: u64 },
    /// The confirmation token does not authorize this plan.
    InvalidConfirmation,
    Internal(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadPath { path, reason } => write!(f, "{path} cannot be used: {reason}"),
            Self::Overlapping { source, destination } => {
                write!(f, "{source} and {destination} contain one another")
            }
            Self::NotEnoughSpace { needed, available } => {
                write!(f, "{needed} bytes needed, {available} available")
            }
            Self::InvalidConfirmation => write!(f, "this plan is no longer valid; re-check it"),
            Self::Internal(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for SyncError {}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// Running totals while walking the source.
#[derive(Debug, Default)]
struct Tally {
    total_to_copy: u64,
    bytes_to_copy: u64,
    already_present: u64,
    differing_skipped: u64,
    special_skipped: u64,
    unreadable: u64,
}

fn display(path: &Path) -> DisplayPath {
    DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes())
}

/// True when `path` is absolute and free of `..`.
fn acceptable(path: &Path) -> bool {
    path.is_absolute() && !path.components().any(|part| matches!(part, Component::ParentDir))
}

/// Rejects a source and destination that are the same tree or nested.
///
/// Resolved, not lexical. Two paths that share no components can name the same
/// directory through a symlink, and syncing a tree into itself would walk its
/// own output — the same trap that
/// [`crate::relocate`] hit and the reason that one canonicalizes too.
fn check_disjoint(source: &Path, destination: &Path) -> Result<(), SyncError> {
    let overlapping = |a: &Path, b: &Path| a == b || b.starts_with(a) || a.starts_with(b);
    let fail = || SyncError::Overlapping { source: display(source), destination: display(destination) };

    if overlapping(source, destination) {
        return Err(fail());
    }
    match (source.canonicalize(), destination.canonicalize()) {
        (Ok(real_source), Ok(real_destination)) if overlapping(&real_source, &real_destination) => Err(fail()),
        _ => Ok(()),
    }
}

/// Bytes free and filesystem type for the mount holding `path`.
fn filesystem_facts(path: &Path) -> (u64, String) {
    let Ok(output) = Command::new("/bin/df").args(["-Pk", "-Y"]).arg(path).output() else {
        return (0, "unknown".to_owned());
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text.lines().nth(1).unwrap_or_default().split_whitespace().collect();
    // Filesystem, Type, 1024-blocks, Used, Available, Capacity, Mounted-on
    let available = fields
        .get(4)
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(0, |blocks| blocks.saturating_mul(1_024));
    let kind = fields.get(1).copied().unwrap_or("unknown").to_ascii_lowercase();
    (available, kind)
}

fn carries_macos_metadata(kind: &str) -> bool {
    kind == "apfs" || kind.starts_with("hfs")
}

/// Compares two files byte for byte.
///
/// Streaming rather than hashing because both sides are local: reading both
/// and comparing costs the same I/O a hash-both-sides scheme would, has no
/// collision to argue about, and needs no hashing crate.
fn contents_differ(a: &Path, b: &Path) -> io::Result<bool> {
    let mut left = fs::File::open(a)?;
    let mut right = fs::File::open(b)?;
    let mut buffer_a = vec![0_u8; COMPARE_CHUNK];
    let mut buffer_b = vec![0_u8; COMPARE_CHUNK];
    loop {
        let read_a = read_full(&mut left, &mut buffer_a)?;
        let read_b = read_full(&mut right, &mut buffer_b)?;
        if read_a != read_b {
            return Ok(true);
        }
        if read_a == 0 {
            return Ok(false);
        }
        if buffer_a[..read_a] != buffer_b[..read_b] {
            return Ok(true);
        }
    }
}

/// Fills `buffer` unless EOF comes first.
///
/// `Read::read` may return short at any time, and comparing two short reads of
/// different lengths from identical files would be a false difference — which
/// in a sync means copying a file that did not need copying.
fn read_full(file: &mut fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Walks the source and decides what the destination is missing.
///
/// `cap` bounds how many entries are RECORDED; the counts in `tally` are always
/// complete. Only the listing is capped, and only because a plan is a review
/// surface — the apply path passes `usize::MAX`, because a display limit must
/// never become a silent copy limit.
///
/// Only the SOURCE is enumerated. For each of its files the matching
/// destination path is `stat`ed directly, which means the destination is never
/// walked and files that exist only there are never even looked at — correct,
/// because this operation never touches them. It also keeps memory flat: there
/// is no map of the destination tree, just one `stat` per source file.
fn walk(
    source_root: &Path,
    destination_root: &Path,
    relative: &Path,
    mode: CompareMode,
    on_differ: OnDiffer,
    cap: usize,
    entries: &mut Vec<SyncEntry>,
    tally: &mut Tally,
) {
    let here = source_root.join(relative);
    let Ok(read) = fs::read_dir(&here) else {
        tally.unreadable = tally.unreadable.saturating_add(1);
        return;
    };

    // Sorted so a plan is stable between runs and reviewable — a directory
    // listing is in whatever order the filesystem felt like.
    let mut children: BTreeMap<std::ffi::OsString, fs::DirEntry> = BTreeMap::new();
    for entry in read.flatten() {
        children.insert(entry.file_name(), entry);
    }

    for (name, entry) in children {
        if SKIPPED_DIRECTORY_NAMES.iter().any(|skip| name == *skip) {
            continue;
        }
        let child_relative = relative.join(&name);
        let Ok(kind) = entry.file_type() else {
            tally.unreadable = tally.unreadable.saturating_add(1);
            continue;
        };

        if kind.is_dir() {
            walk(source_root, destination_root, &child_relative, mode, on_differ, cap, entries, tally);
            continue;
        }

        // A socket or FIFO cannot be copied by anything. Counted so the plan
        // can say the destination will not be a byte-for-byte twin, rather
        // than failing the whole sync over something no tool can carry.
        if !kind.is_file() && !kind.is_symlink() {
            tally.special_skipped = tally.special_skipped.saturating_add(1);
            continue;
        }

        let source_path = source_root.join(&child_relative);
        let destination_path = destination_root.join(&child_relative);
        let Ok(source_meta) = fs::symlink_metadata(&source_path) else {
            tally.unreadable = tally.unreadable.saturating_add(1);
            continue;
        };
        let size = source_meta.len();

        let reason = match fs::symlink_metadata(&destination_path) {
            Err(_) => Some(SyncReason::Missing),
            Ok(destination_meta) => {
                if destination_meta.len() != size {
                    Some(SyncReason::SizeDiffers)
                } else if mode == CompareMode::Verify
                    && kind.is_file()
                    && contents_differ(&source_path, &destination_path).unwrap_or(false)
                {
                    Some(SyncReason::ContentDiffers)
                } else {
                    None
                }
            }
        };

        let Some(reason) = reason else {
            tally.already_present = tally.already_present.saturating_add(1);
            continue;
        };

        // A file that exists and differs is only copied when the user has
        // asked for that. Skipping is the default because overwriting is the
        // one thing here that destroys something.
        if reason != SyncReason::Missing && on_differ == OnDiffer::Skip {
            tally.differing_skipped = tally.differing_skipped.saturating_add(1);
            continue;
        }

        tally.total_to_copy = tally.total_to_copy.saturating_add(1);
        tally.bytes_to_copy = tally.bytes_to_copy.saturating_add(size);
        if entries.len() < cap {
            entries.push(SyncEntry {
                relative_path: child_relative.display().to_string(),
                bytes: size,
                reason,
            });
        }
    }
}

/// Describes what a sync would do, and mints the token that authorizes it.
///
/// Returns a plan even when the sync cannot proceed, for the same reason
/// [`crate::relocate::plan`] does: the reasons are the useful output, and a UI
/// that can only render success renders nothing at the moment the user most
/// needs an explanation. `token: None` means not actionable.
///
/// # Errors
///
/// Only for a request that cannot be described at all — a relative path, a
/// path that is not a directory, or two paths that overlap.
pub(crate) fn plan<S: BuildHasher>(
    keys: &S,
    generation: TreeGeneration,
    now_unix_ms: i64,
    source: &Path,
    destination: &Path,
    compare_mode: CompareMode,
    on_differ: OnDiffer,
) -> Result<SyncPlan, SyncError> {
    for path in [source, destination] {
        if !acceptable(path) {
            return Err(SyncError::BadPath {
                path: display(path),
                reason: "it must be an absolute path with no `..` segments".to_owned(),
            });
        }
        if !path.is_dir() {
            return Err(SyncError::BadPath {
                path: display(path),
                reason: "it is not a directory".to_owned(),
            });
        }
    }
    check_disjoint(source, destination)?;

    let mut entries = Vec::new();
    let mut tally = Tally::default();
    walk(source, destination, Path::new(""), compare_mode, on_differ, MAX_PLANNED_ENTRIES, &mut entries, &mut tally);

    let (destination_available, destination_filesystem) = filesystem_facts(destination);

    let mut warnings = Vec::new();
    if tally.total_to_copy == 0 {
        warnings.push(SyncWarning::new(
            "nothing-to-do",
            "The destination already has everything in the source. Nothing would be copied.",
        ));
    }
    if !carries_macos_metadata(&destination_filesystem) {
        // A WARNING here, not a refusal — unlike relocate, which deletes the
        // source and so makes metadata loss permanent. A sync leaves the
        // original in place, so the user can always copy it somewhere better.
        warnings.push(SyncWarning::new(
            "metadata-loss",
            format!(
                "The destination is {destination_filesystem}, which cannot store extended \
                 attributes or ACLs. Finder tags, colour labels and \"Where from\" provenance will \
                 not survive the copy. The originals keep theirs."
            ),
        ));
    }
    if tally.special_skipped > 0 {
        warnings.push(SyncWarning::new(
            "special-files",
            format!(
                "{} socket{} or pipe{} will be skipped. Nothing can copy those.",
                tally.special_skipped,
                if tally.special_skipped == 1 { "" } else { "s" },
                if tally.special_skipped == 1 { "" } else { "s" },
            ),
        ));
    }
    if tally.unreadable > 0 {
        warnings.push(SyncWarning::new(
            "unreadable",
            format!(
                "{} item{} could not be read, so their contents are not in this plan. The counts \
                 below are a floor.",
                tally.unreadable,
                if tally.unreadable == 1 { "" } else { "s" },
            ),
        ));
    }
    if tally.differing_skipped > 0 && on_differ == OnDiffer::Skip {
        warnings.push(SyncWarning::new(
            "differing-skipped",
            format!(
                "{} file{} exist on both sides but differ. They are being left alone — switch to \
                 \"replace\" if the source should win.",
                tally.differing_skipped,
                if tally.differing_skipped == 1 { "" } else { "s" },
            ),
        ));
    }

    let short = tally.bytes_to_copy > destination_available;
    if short {
        warnings.push(SyncWarning::new(
            "no-room",
            format!(
                "The destination has {destination_available} bytes free and this needs {}.",
                tally.bytes_to_copy
            ),
        ));
    }

    // The token binds the generation and the plan's shape. It is deliberately
    // NOT bound to a node's (dev, ino) the way relocate's is: a sync has no
    // single subject, and what must not drift between plan and apply is the
    // set of paths and the counts the user reviewed.
    let identity = ItemIdentity {
        node: rdirstat_core::NodeId::from_raw(0),
        device: tally.total_to_copy,
        inode: tally.bytes_to_copy,
    };
    let actionable = tally.total_to_copy > 0 && !short;
    let token = actionable.then(|| token::mint(keys, generation, now_unix_ms, &[identity]));

    Ok(SyncPlan {
        generation,
        token,
        source: display(source),
        destination: display(destination),
        compare_mode,
        on_differ,
        entries_truncated: tally.total_to_copy > entries.len() as u64,
        entries,
        total_to_copy: tally.total_to_copy,
        bytes_to_copy: tally.bytes_to_copy,
        already_present: tally.already_present,
        differing_skipped: tally.differing_skipped,
        special_skipped: tally.special_skipped,
        unreadable: tally.unreadable,
        destination_available,
        destination_filesystem,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

/// Copies one file, creating the directories above it.
///
/// `ditto` per file rather than once for the tree: a whole-tree `ditto` would
/// copy everything, including the files the destination already has, which is
/// the opposite of what was asked for. Per-file is more process spawns and
/// exactly the semantics the user described.
fn copy_one(source: &Path, destination: &Path) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let output = Command::new("/usr/bin/ditto")
        .arg("--rsrc")
        .arg("--extattr")
        .arg("--acl")
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| format!("could not run ditto: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "ditto exited {}: {}",
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Runs a planned sync.
///
/// Re-plans from scratch rather than trusting the entry list it was handed:
/// the frontend's copy of the plan is display state, and acting on a list of
/// paths supplied by the webview would be acting on attacker-controlled input.
/// The token is then checked against the freshly computed shape, so a source
/// that changed between the review and the confirm fails closed.
///
/// A failure on one file does NOT abort the rest. Each copy is independent,
/// and stopping would leave a half-synced destination with no record of which
/// half — the same reasoning as the batch move.
///
/// # Errors
///
/// [`SyncError::InvalidConfirmation`] if the plan no longer matches, or any of
/// the path errors from [`plan`].
pub(crate) fn apply<S: BuildHasher>(
    keys: &S,
    generation: TreeGeneration,
    now_unix_ms: i64,
    source: &Path,
    destination: &Path,
    compare_mode: CompareMode,
    on_differ: OnDiffer,
    confirmation: &ConfirmationToken,
) -> Result<SyncReport, SyncError> {
    let fresh = plan(keys, generation, now_unix_ms, source, destination, compare_mode, on_differ)?;

    let identity = ItemIdentity {
        node: rdirstat_core::NodeId::from_raw(0),
        device: fresh.total_to_copy,
        inode: fresh.bytes_to_copy,
    };
    token::verify(keys, confirmation, generation, now_unix_ms, &[identity])
        .map_err(|_| SyncError::InvalidConfirmation)?;

    let mut report = SyncReport {
        generation,
        source: display(source),
        destination: display(destination),
        copied: 0,
        bytes_copied: 0,
        failures: Vec::new(),
    };

    // Re-walked UNCAPPED rather than using `fresh.entries`, which is
    // truncated for display. A plan that listed 5,000 of 40,000 files must
    // still copy all 40,000.
    let mut all = Vec::new();
    let mut tally = Tally::default();
    walk(source, destination, Path::new(""), compare_mode, on_differ, usize::MAX, &mut all, &mut tally);

    for entry in all {
        let relative = Path::new(&entry.relative_path);
        match copy_one(&source.join(relative), &destination.join(relative)) {
            Ok(()) => {
                report.copied = report.copied.saturating_add(1);
                report.bytes_copied = report.bytes_copied.saturating_add(entry.bytes);
            }
            Err(reason) => report.failures.push(SyncFailure {
                relative_path: entry.relative_path,
                reason,
            }),
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;

    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const GEN: TreeGeneration = TreeGeneration::FIRST;

    /// Scratch on ordinary ground, with no `..` in the path — `acceptable`
    /// rejects those, and `CARGO_MANIFEST_DIR/../target` literally contains one.
    fn scratch() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/sync-scratch");
        fs::create_dir_all(&base).expect("scratch root");
        let base = base.canonicalize().expect("canonicalize");
        tempfile::Builder::new().prefix("case-").tempdir_in(&base).expect("tempdir")
    }

    fn write(root: &Path, relative: &str, contents: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, contents).expect("write");
    }

    fn plan_of(source: &Path, destination: &Path, mode: CompareMode, on_differ: OnDiffer) -> SyncPlan {
        let keys = RandomState::new();
        plan(&keys, GEN, NOW, source, destination, mode, on_differ).expect("plan")
    }

    #[test]
    fn a_file_the_destination_lacks_is_the_thing_to_copy() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "a.txt", b"one");
        write(source.path(), "nested/b.txt", b"two");
        write(destination.path(), "a.txt", b"one");

        let plan = plan_of(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        assert_eq!(plan.total_to_copy, 1);
        assert_eq!(plan.already_present, 1);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].relative_path, "nested/b.txt");
        assert_eq!(plan.entries[0].reason, SyncReason::Missing);
    }

    #[test]
    fn nothing_in_the_destination_is_ever_proposed_for_removal() {
        // The whole safety posture: a file that exists ONLY in the destination
        // is not in the plan, not counted, and never looked at again.
        let source = scratch();
        let destination = scratch();
        write(source.path(), "shared.txt", b"x");
        write(destination.path(), "shared.txt", b"x");
        write(destination.path(), "only-here.txt", b"precious");

        let plan = plan_of(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        assert_eq!(plan.total_to_copy, 0);
        assert!(
            !plan.entries.iter().any(|entry| entry.relative_path.contains("only-here")),
            "a destination-only file must never appear in a sync plan"
        );
    }

    #[test]
    fn a_differing_file_is_left_alone_unless_replacing_is_asked_for() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "a.txt", b"new contents");
        write(destination.path(), "a.txt", b"old");

        let skip = plan_of(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        assert_eq!(skip.total_to_copy, 0, "skipping must not queue a copy");
        assert_eq!(skip.differing_skipped, 1);
        assert!(skip.warnings.iter().any(|warning| warning.code == "differing-skipped"));

        let replace = plan_of(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Replace);
        assert_eq!(replace.total_to_copy, 1);
        assert_eq!(replace.entries[0].reason, SyncReason::SizeDiffers);
    }

    #[test]
    fn same_size_different_bytes_is_invisible_to_quick_and_caught_by_verify() {
        // The case that justifies offering Verify at all: a file edited in
        // place without changing length. Quick calls it identical, which is
        // the documented trade, and Verify catches it.
        let source = scratch();
        let destination = scratch();
        write(source.path(), "a.bin", b"AAAAAAAA");
        write(destination.path(), "a.bin", b"BBBBBBBB");

        let quick = plan_of(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Replace);
        assert_eq!(quick.total_to_copy, 0);
        assert_eq!(quick.already_present, 1);

        let verify = plan_of(source.path(), destination.path(), CompareMode::Verify, OnDiffer::Replace);
        assert_eq!(verify.total_to_copy, 1);
        assert_eq!(verify.entries[0].reason, SyncReason::ContentDiffers);
    }

    #[test]
    fn a_multi_chunk_identical_file_is_not_a_false_difference() {
        // Exercises the fill-to-boundary loop: a naive single read per side can
        // return different lengths for identical files, and in a sync that
        // means copying something that did not need copying.
        let source = scratch();
        let destination = scratch();
        let payload: Vec<u8> = (0..(COMPARE_CHUNK * 2 + 517))
            .map(|i| u8::try_from(i % 251).expect("a value mod 251 fits in a u8"))
            .collect();
        write(source.path(), "big.bin", &payload);
        write(destination.path(), "big.bin", &payload);

        let plan = plan_of(source.path(), destination.path(), CompareMode::Verify, OnDiffer::Replace);
        assert_eq!(plan.total_to_copy, 0, "identical multi-chunk files must compare equal");
    }

    #[test]
    fn a_destination_inside_the_source_is_refused() {
        // Syncing a tree into itself would walk its own output.
        let dir = scratch();
        let source = dir.path().join("tree");
        let destination = source.join("backup");
        fs::create_dir_all(&destination).expect("mkdir");
        let keys = RandomState::new();

        let error = plan(&keys, GEN, NOW, &source, &destination, CompareMode::Quick, OnDiffer::Skip)
            .expect_err("nesting must be refused");
        assert!(matches!(error, SyncError::Overlapping { .. }), "got {error:?}");
    }

    #[test]
    fn a_destination_reaching_the_source_through_a_symlink_is_refused() {
        // Lexically disjoint, physically the same tree.
        let dir = scratch();
        let real = dir.path().join("real");
        fs::create_dir_all(&real).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let keys = RandomState::new();

        assert!(
            !link.starts_with(&real) && !real.starts_with(&link),
            "the two must look disjoint lexically or this proves nothing"
        );
        let error = plan(&keys, GEN, NOW, &real, &link, CompareMode::Quick, OnDiffer::Skip)
            .expect_err("must refuse");
        assert!(matches!(error, SyncError::Overlapping { .. }), "got {error:?}");
    }

    #[test]
    fn a_relative_path_is_refused() {
        let dir = scratch();
        let keys = RandomState::new();
        assert!(
            plan(&keys, GEN, NOW, Path::new("relative"), dir.path(), CompareMode::Quick, OnDiffer::Skip)
                .is_err()
        );
    }

    #[test]
    fn applying_copies_only_the_missing_files_and_leaves_the_rest() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "keep.txt", b"same");
        write(source.path(), "deep/new.txt", b"brand new");
        write(destination.path(), "keep.txt", b"same");
        write(destination.path(), "theirs.txt", b"do not touch");

        let keys = RandomState::new();
        let plan = plan(&keys, GEN, NOW, source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip)
            .expect("plan");
        let token = plan.token.clone().expect("a copyable plan must get a token");

        let report = apply(
            &keys,
            GEN,
            NOW,
            source.path(),
            destination.path(),
            CompareMode::Quick,
            OnDiffer::Skip,
            &token,
        )
        .expect("apply");

        assert_eq!(report.copied, 1);
        assert!(report.failures.is_empty(), "got {:?}", report.failures);
        // The missing file arrived, directories and all.
        assert_eq!(fs::read(destination.path().join("deep/new.txt")).expect("read"), b"brand new");
        // And nothing else moved.
        assert_eq!(fs::read(destination.path().join("keep.txt")).expect("read"), b"same");
        assert_eq!(fs::read(destination.path().join("theirs.txt")).expect("read"), b"do not touch");
    }

    #[test]
    fn a_plan_with_nothing_to_do_is_not_actionable() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "a.txt", b"x");
        write(destination.path(), "a.txt", b"x");

        let plan = plan_of(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        assert!(plan.token.is_none(), "there is nothing to authorize");
        assert!(plan.warnings.iter().any(|warning| warning.code == "nothing-to-do"));
    }

    #[test]
    fn a_token_does_not_survive_the_source_changing() {
        // The plan is reviewed, then the source gains a file. The token was
        // minted against the old shape, so the confirm fails closed rather
        // than copying something the user never saw counted.
        let source = scratch();
        let destination = scratch();
        write(source.path(), "a.txt", b"one");

        let keys = RandomState::new();
        let plan = plan(&keys, GEN, NOW, source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip)
            .expect("plan");
        let token = plan.token.clone().expect("token");

        write(source.path(), "surprise.txt", b"appeared after review");

        let error = apply(
            &keys,
            GEN,
            NOW,
            source.path(),
            destination.path(),
            CompareMode::Quick,
            OnDiffer::Skip,
            &token,
        )
        .expect_err("a changed source must invalidate the plan");
        assert!(matches!(error, SyncError::InvalidConfirmation), "got {error:?}");
    }
}
