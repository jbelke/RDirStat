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

// Reached through the Tauri commands that expose it. `commands.rs` is held by
// another session right now, so for the moment these are used only by the tests
// below — which is what `dead_code` is reporting. Comes out when the commands
// land; it is a scaffold, not a policy.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::BuildHasher;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
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
///
/// Defined in `rdirstat-remote` and re-exported here, so the local and the
/// remote planner share one type rather than two identical ones. That is not
/// tidiness: `specta` generates a TypeScript type per Rust type, and two called
/// `OnDiffer` would collide in `bindings.ts`. The serde representation is
/// unchanged — `"skip"` and `"replace"` on the wire, exactly as before.
pub(crate) use rdirstat_remote::plan::OnDiffer;

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

/// One planned copy, with the path the filesystem actually uses.
///
/// [`SyncEntry::relative_path`] is a `String` because it crosses IPC, and it is
/// built with `Path::display()`, which replaces invalid UTF-8 with U+FFFD. That
/// is fine for showing a user, and unusable for addressing a file: two distinct
/// names can collapse to the SAME string, and joining that string back onto the
/// root can name a file that does not exist — or, worse, a different one.
///
/// So everything that touches the filesystem uses `relative`, and only the
/// display string crosses the wire.
#[derive(Clone, Debug)]
struct PlannedCopy {
    relative: PathBuf,
    entry: SyncEntry,
}

/// Something the user should read before confirming.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct SyncWarning {
    pub code: String,
    pub message: String,
}

impl SyncWarning {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
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
    BadPath {
        path: DisplayPath,
        reason: String,
    },
    /// Source and destination are the same tree, or one contains the other.
    Overlapping {
        source: DisplayPath,
        destination: DisplayPath,
    },
    /// The destination does not have room for what would be copied.
    NotEnoughSpace {
        needed: u64,
        available: u64,
    },
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

/// What the caller wants synced.
///
/// Bundled so `plan` and `apply` take the same four things and cannot be
/// called with source and destination the wrong way round — which, for an
/// operation that writes into one of them, is the argument-order mistake worth
/// designing out.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SyncRequest<'a> {
    pub source: &'a Path,
    pub destination: &'a Path,
    pub compare_mode: CompareMode,
    pub on_differ: OnDiffer,
}

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
    let fail = || SyncError::Overlapping {
        source: display(source),
        destination: display(destination),
    };

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
    request: SyncRequest<'_>,
    relative: &Path,
    cap: usize,
    entries: &mut Vec<PlannedCopy>,
    tally: &mut Tally,
) {
    let (source_root, destination_root) = (request.source, request.destination);
    let (mode, on_differ) = (request.compare_mode, request.on_differ);
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
            walk(request, &child_relative, cap, entries, tally);
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
            entries.push(PlannedCopy {
                entry: SyncEntry {
                    relative_path: child_relative.display().to_string(),
                    bytes: size,
                    reason,
                },
                relative: child_relative.clone(),
            });
        }
    }
}

/// Everything worth saying about a plan before the user confirms it.
///
/// Lifted out of [`plan`] for length, and because it is a flat list of
/// observations rather than part of the decision — the decisions above it were
/// getting hard to see behind five `if` blocks of prose.
fn advisories(
    tally: &Tally,
    filesystem: &str,
    available: u64,
    on_differ: OnDiffer,
    short: bool,
) -> Vec<SyncWarning> {
    let mut warnings = Vec::new();
    let plural = |count: u64| if count == 1 { "" } else { "s" };

    if tally.total_to_copy == 0 {
        warnings.push(SyncWarning::new(
            "nothing-to-do",
            "The destination already has everything in the source. Nothing would be copied.",
        ));
    }

    // A WARNING, not a refusal — unlike relocate, which deletes the source and
    // so makes metadata loss permanent. A sync leaves the original in place, so
    // the user can always copy it somewhere better afterwards.
    if !carries_macos_metadata(filesystem) {
        warnings.push(SyncWarning::new(
            "metadata-loss",
            format!(
                "The destination is {filesystem}, which cannot store extended attributes or ACLs. \
                 Finder tags, colour labels and \"Where from\" provenance will not survive the \
                 copy. The originals keep theirs."
            ),
        ));
    }

    if tally.special_skipped > 0 {
        warnings.push(SyncWarning::new(
            "special-files",
            format!(
                "{} socket{} or pipe{} will be skipped. Nothing can copy those.",
                tally.special_skipped,
                plural(tally.special_skipped),
                plural(tally.special_skipped),
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
                plural(tally.unreadable),
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
                plural(tally.differing_skipped),
            ),
        ));
    }

    if short {
        warnings.push(SyncWarning::new(
            "no-room",
            format!(
                "The destination has {available} bytes free and this needs {}.",
                tally.bytes_to_copy
            ),
        ));
    }

    warnings
}

/// Rejects a request that cannot be acted on at all.
///
/// Shared by [`plan`] and [`apply`] so the two cannot drift: an apply that
/// validated less than the plan it is authorised by would be a way in.
fn usable(request: SyncRequest<'_>) -> Result<(), SyncError> {
    for path in [request.source, request.destination] {
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
    check_disjoint(request.source, request.destination)
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
    request: SyncRequest<'_>,
) -> Result<SyncPlan, SyncError> {
    let SyncRequest { source, destination, compare_mode, on_differ } = request;
    usable(request)?;

    let mut entries = Vec::new();
    let mut tally = Tally::default();
    walk(request, Path::new(""), MAX_PLANNED_ENTRIES, &mut entries, &mut tally);

    let (destination_available, destination_filesystem) = filesystem_facts(destination);

    let short = tally.bytes_to_copy > destination_available;
    let warnings = advisories(&tally, &destination_filesystem, destination_available, on_differ, short);

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
        entries: entries.into_iter().map(|planned| planned.entry).collect(),
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
/// The slow path. [`copy_planned`] copies the whole planned set in one `ditto`
/// and is what runs normally; this remains for the two cases a bom cannot
/// serve — a relative path containing a newline, and reporting per-file
/// failures after a batch copy has failed as a whole.
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

/// A private scratch directory that removes itself.
///
/// `create_dir`, not `create_dir_all`: it fails when the path already exists,
/// so a directory somebody else pre-created is an error rather than something
/// this writes through.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path = std::env::temp_dir().join(format!("rdirstat-sync-{}-{nanos}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

/// True when a relative path can go into a bom at all.
///
/// `mkbom -i` reads its file list one path per line and offers no NUL-delimited
/// form, so a name containing a newline would be split into two bogus entries.
/// That is precisely the defect filed against `rgigasync` as nato-b6h.3, and it
/// is not worth reintroducing for the handful of files it affects: those take
/// the per-file path instead.
fn bom_can_carry(relative: &Path) -> bool {
    // Two independent reasons a path cannot go in a bom, and both must be
    // checked against the REAL path rather than its display string.
    //
    // `mkbom -i` reads its list one path per line with no NUL-delimited form,
    // so a newline would split one entry into two bogus ones — the defect filed
    // against `rgigasync` as nato-b6h.3.
    //
    // And a bom is a text file, so a name that is not valid UTF-8 cannot be
    // written into one faithfully. Such a name is perfectly copyable one file
    // at a time, because that path never round-trips through a string.
    match relative.to_str() {
        Some(text) => !text.contains('\n'),
        None => false,
    }
}

/// The `mkbom` input for a planned set: every path, every directory above it,
/// and the root.
///
/// A `BTreeSet` because `mkbom` walks the list in order and rejects an entry
/// whose parent it has not already seen — and lexicographic order puts `.`
/// ahead of everything and `./a` ahead of `./a/b`. The sort is not cosmetic; it
/// is what makes the bom build at all.
fn bom_lines(entries: &[PlannedCopy]) -> BTreeSet<String> {
    let mut lines = BTreeSet::new();
    lines.insert(".".to_owned());
    for entry in entries {
        // Every ancestor, because a bom naming `./a/b/c.txt` without `./a` and
        // `./a/b` has those entries rejected one by one and yields a bom that
        // copies less than it was asked to, while exiting 0.
        let mut prefix = PathBuf::new();
        for part in entry.relative.components() {
            prefix.push(part);
            lines.insert(format!("./{}", prefix.display()));
        }
    }
    lines
}

/// Copies exactly the planned set in one `ditto`.
///
/// ## Why a bom
///
/// The two obvious options are both bad. One `ditto` over the tree copies
/// everything, including the files the destination already has, which is the
/// opposite of what was asked for. One `ditto` per file is correct but spends
/// most of itself in `fork`/`exec`: measured on this project's bench, 20,000
/// files cross-volume cost 109.87 s per-file serially and 44.09 s at sixteen-way
/// parallelism, against 11.01 s for a single bom-driven copy — and a bare
/// `/usr/bin/true` costs 2.56 ms to spawn, so no amount of parallelism removes
/// that floor.
///
/// `ditto --bom` takes a manifest and copies *only* the objects named in it, at
/// full fidelity. It is selectivity without the per-file spawn, and it keeps
/// `ditto` as the copier, so extended attributes, ACLs and resource forks
/// survive — the property [`crate::relocate`] is built on. It needs no `rsync`,
/// so it needs no capability probe: the system `rsync` on macOS 15 is openrsync
/// and rejects `--xattrs` and `--acls` outright.
///
/// Verified semantics: a file already at the destination and named in the bom
/// is overwritten; one that is *not* named is left completely alone.
///
/// # Errors
///
/// Returns the reason when the scratch directory, `mkbom` or `ditto` fails. The
/// caller falls back to per-file copies so the report can still say which files
/// did not make it.
fn copy_planned(source: &Path, destination: &Path, entries: &[PlannedCopy]) -> Result<(), String> {
    let lines = bom_lines(entries);
    let scratch = Scratch::new().map_err(|error| format!("could not create a scratch directory: {error}"))?;
    let list = scratch.0.join("list");
    let bom = scratch.0.join("sync.bom");

    let mut text = String::new();
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    fs::write(&list, text).map_err(|error| format!("could not write the copy list: {error}"))?;

    let built = Command::new("/usr/bin/mkbom")
        .arg("-s")
        .arg("-i")
        .arg(&list)
        .arg(&bom)
        // `mkbom` resolves the listed paths against the working directory, so
        // it has to run inside the source.
        .current_dir(source)
        .output()
        .map_err(|error| format!("could not run mkbom: {error}"))?;
    if !built.status.success() {
        return Err(format!(
            "mkbom exited {}: {}",
            built.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&built.stderr).trim()
        ));
    }

    // `mkbom` reports a rejected entry on stderr and still exits 0, so this
    // counts the bom back rather than trusting that exit status.
    //
    // Being precise about what this does NOT do, because an earlier version of
    // this comment overstated it: `mkbom -s -i` never stats the listed paths,
    // and `bom_lines` already guarantees the one rejection rule (a parent
    // preceding its children) by construction, so on today's inputs this cannot
    // fire. It is kept as a regression guard on `bom_lines`, not as the check
    // that the copy was complete. A bom naming a path that is not in the source
    // passes here and is silently skipped by `ditto` — that case is caught
    // after the copy, by reconciling against the filesystem in `apply`.
    let listed = Command::new("/usr/bin/lsbom")
        .arg("-s")
        .arg(&bom)
        .output()
        .map_err(|error| format!("could not run lsbom: {error}"))?;
    if !listed.status.success() {
        return Err("the copy list could not be read back".to_owned());
    }
    let built_count = String::from_utf8_lossy(&listed.stdout).lines().filter(|line| !line.is_empty()).count();
    if built_count != lines.len() {
        return Err(format!(
            "the copy list describes {built_count} of {} paths; refusing to copy a partial list",
            lines.len()
        ));
    }

    let copied = Command::new("/usr/bin/ditto")
        .arg("--bom")
        .arg(&bom)
        .arg("--rsrc")
        .arg("--extattr")
        .arg("--acl")
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| format!("could not run ditto: {error}"))?;
    if copied.status.success() {
        return Ok(());
    }
    Err(format!(
        "ditto exited {}: {}",
        copied.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&copied.stderr).trim()
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
    request: SyncRequest<'_>,
    confirmation: &ConfirmationToken,
) -> Result<SyncReport, SyncError> {
    let SyncRequest { source, destination, .. } = request;
    usable(request)?;

    // ONE walk, uncapped. `plan` caps its entry list for display, so this used
    // to call `plan` and then walk a second time to get the full set — which
    // under `CompareMode::Verify` byte-compared the entire overlap TWICE
    // before the first byte was copied. The walk already produces both the
    // entries and the tally the token is checked against, so the second pass
    // was buying nothing.
    let mut all = Vec::new();
    let mut tally = Tally::default();
    walk(request, Path::new(""), usize::MAX, &mut all, &mut tally);

    // Re-derived from THIS walk, not from the caller's plan: the frontend's
    // copy of the plan is display state, and a token that verified against
    // numbers supplied by the webview would authorise nothing. A source that
    // changed between the review and the confirm fails closed here.
    let identity = ItemIdentity {
        node: rdirstat_core::NodeId::from_raw(0),
        device: tally.total_to_copy,
        inode: tally.bytes_to_copy,
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

    // The planned set goes into ONE `ditto --bom`; see `copy_planned`. Only the
    // paths a bom cannot describe fall back to a process each.
    let (batchable, individual): (Vec<PlannedCopy>, Vec<PlannedCopy>) =
        all.into_iter().partition(|planned| bom_can_carry(&planned.relative));

    if !batchable.is_empty()
        && let Err(reason) = copy_planned(source, destination, &batchable)
    {
        report.failures.push(SyncFailure {
            relative_path: String::new(),
            reason: format!("copying in one pass failed, falling back to one file at a time: {reason}"),
        });
    }

    // Reconcile against the FILESYSTEM, never against the batch's exit code.
    //
    // `ditto --bom` exits 0 for a bom naming a path that is not in the source —
    // it simply skips it. That is reachable two ways: the tree can change
    // between the walk and the copy, which is the whole reason `apply` re-walks;
    // and `relative_path` comes from `Path::display()`, which replaces invalid
    // UTF-8 with U+FFFD, so a name that is not valid UTF-8 becomes a path that
    // does not exist. Crediting the batch on its exit status would report those
    // files as copied when the destination never received them — a report that
    // lies, which is worse than a failure.
    //
    // So every planned entry is checked for having actually landed. The ones
    // that did cost a `stat` and are counted; the ones that did not are copied
    // one at a time, which either succeeds or produces a per-file failure with
    // a real reason. This is also what keeps a partially-succeeded batch from
    // re-sending the bytes it already delivered.
    copy_individually(source, destination, batchable, &mut report);
    copy_individually(source, destination, individual, &mut report);

    Ok(report)
}

/// True when a file the fallback is about to copy is already at the destination.
///
/// The fallback exists to find out WHICH files failed after a batch copy failed
/// as a whole. But `ditto` is not all-or-nothing: it can exit non-zero having
/// already written most of a tree, which is what a full disk, a mid-walk
/// permission denial or an unmounted volume actually look like. Without this
/// check the fallback would re-copy every byte the batch had already landed —
/// a 500 GB transfer that failed at 400 GB would serially re-copy all 500 GB,
/// one process per file, on the exact path where the user most wants out.
///
/// Size is a sound test here for a reason worth writing down precisely, because
/// the obvious reason is wrong. It is NOT that a half-written file would have a
/// different size and so be re-copied: `ditto` writes to a hidden `.BC.T_*`
/// temporary beside the target and renames it, so the target path is always
/// either absent or complete, and a short file never appears there at all. On
/// ENOSPC it removes its own temporary. Were `ditto` ever replaced by a copier
/// that writes in place, this function would need a content check, not a
/// bigger size check.
///
/// What makes size sufficient is the plan: an entry is here because the
/// destination lacked the file or had it at a different size, so a destination
/// file at the source's size is one this run put there. The exception is
/// [`SyncReason::ContentDiffers`], which by definition means the sizes already
/// matched and only the bytes differed — for those a size match proves nothing
/// and the copy is redone.
///
/// The comparison is by the entry's REAL path, never its display string; see
/// [`PlannedCopy`] for why that distinction is load-bearing rather than tidy.
fn already_landed(entry: &SyncEntry, target: &Path) -> bool {
    if entry.reason == SyncReason::ContentDiffers {
        return false;
    }
    fs::symlink_metadata(target).is_ok_and(|meta| !meta.is_dir() && meta.len() == entry.bytes)
}

/// Copies entries one process at a time, recording a verdict for each.
fn copy_individually(source: &Path, destination: &Path, entries: Vec<PlannedCopy>, report: &mut SyncReport) {
    for planned in entries {
        let target = destination.join(&planned.relative);
        if already_landed(&planned.entry, &target) {
            report.copied = report.copied.saturating_add(1);
            report.bytes_copied = report.bytes_copied.saturating_add(planned.entry.bytes);
            continue;
        }
        match copy_one(&source.join(&planned.relative), &target) {
            Ok(()) => {
                report.copied = report.copied.saturating_add(1);
                report.bytes_copied = report.bytes_copied.saturating_add(planned.entry.bytes);
            }
            Err(reason) => report.failures.push(SyncFailure {
                relative_path: planned.entry.relative_path,
                reason,
            }),
        }
    }
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
        tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(&base)
            .expect("tempdir")
    }

    fn write(root: &Path, relative: &str, contents: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, contents).expect("write");
    }

    fn request<'a>(
        source: &'a Path,
        destination: &'a Path,
        compare_mode: CompareMode,
        on_differ: OnDiffer,
    ) -> SyncRequest<'a> {
        SyncRequest { source, destination, compare_mode, on_differ }
    }

    fn planned(relative_path: &str, bytes: u64, reason: SyncReason) -> PlannedCopy {
        PlannedCopy {
            relative: PathBuf::from(relative_path),
            entry: SyncEntry { relative_path: relative_path.to_owned(), bytes, reason },
        }
    }

    fn plan_of(source: &Path, destination: &Path, mode: CompareMode, on_differ: OnDiffer) -> SyncPlan {
        let keys = RandomState::new();
        plan(&keys, GEN, NOW, request(source, destination, mode, on_differ)).expect("plan")
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
            !plan
                .entries
                .iter()
                .any(|entry| entry.relative_path.contains("only-here")),
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

        let verify = plan_of(
            source.path(),
            destination.path(),
            CompareMode::Verify,
            OnDiffer::Replace,
        );
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

        let plan = plan_of(
            source.path(),
            destination.path(),
            CompareMode::Verify,
            OnDiffer::Replace,
        );
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

        let error = plan(&keys, GEN, NOW, request(&source, &destination, CompareMode::Quick, OnDiffer::Skip))
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
        let error = plan(&keys, GEN, NOW, request(&real, &link, CompareMode::Quick, OnDiffer::Skip)).expect_err("must refuse");
        assert!(matches!(error, SyncError::Overlapping { .. }), "got {error:?}");
    }

    #[test]
    fn a_relative_path_is_refused() {
        let dir = scratch();
        let keys = RandomState::new();
        assert!(
            plan(&keys, GEN, NOW, request(Path::new("relative"), dir.path(), CompareMode::Quick, OnDiffer::Skip))
            .is_err()
        );
    }

    /// Two entries that SHARE a display string must not both be credited.
    ///
    /// `SyncEntry::relative_path` is built with `Path::display()`, which
    /// replaces invalid UTF-8 with U+FFFD — so two distinct filenames can
    /// collapse to the same string. The old code addressed files through that
    /// string, which meant the bom deduplicated them to one line, one file
    /// landed, and the reconciliation then credited BOTH: a silent under-copy
    /// with a clean report, the same category as nato-b6h.1.
    ///
    /// `PlannedCopy` carries the real path, so the two are addressed
    /// separately. Asserted here on the reconciliation directly, because the
    /// natural end-to-end fixture cannot be built on this filesystem: APFS
    /// refuses to create a name that is not valid UTF-8 at all (`File::create`
    /// returns `EILSEQ`). exFAT, NTFS and SMB volumes carry such names, and
    /// this app is pointed at those.
    #[test]
    fn two_entries_sharing_a_display_string_are_not_both_credited() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "collide.txt", b"first");

        // Same display string, different real paths. Only the first exists.
        let landed = planned("collide.txt", 5, SyncReason::Missing);
        let mut ghost = planned("collide.txt", 5, SyncReason::Missing);
        ghost.relative = PathBuf::from("other-real-name.txt");

        let mut report = SyncReport {
            generation: GEN,
            source: display(source.path()),
            destination: display(destination.path()),
            copied: 0,
            bytes_copied: 0,
            failures: Vec::new(),
        };
        copy_individually(source.path(), destination.path(), vec![landed, ghost], &mut report);

        assert_eq!(report.copied, 1, "only the entry whose real file exists may be credited");
        assert_eq!(report.failures.len(), 1, "the other must be reported, got {:?}", report.failures);
    }

    /// The report may never claim a file was copied that is not there.
    ///
    /// `ditto --bom` exits 0 for a bom naming a path that is not in the source:
    /// it silently skips it. So a batch can "succeed" having copied less than
    /// the plan, and crediting entries on that exit status produced a report
    /// that lied — the shape of nato-b6h.1, which is what this whole epic was
    /// convened over.
    ///
    /// Reproduces it directly: two planned entries, only one of which exists in
    /// the source. The batch succeeds. The one that landed must be counted; the
    /// one that never existed must be reported as a failure, not as a copy.
    #[test]
    fn a_batch_that_silently_skipped_a_file_is_not_reported_as_copied() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "real.txt", b"here");

        let entries = vec![
            planned("real.txt", 4, SyncReason::Missing),
            planned("ghost.txt", 9, SyncReason::Missing),
        ];

        // The batch reports success even though `ghost.txt` is not there.
        copy_planned(source.path(), destination.path(), &entries).expect("ditto skips the absent path and exits 0");
        assert!(destination.path().join("real.txt").exists(), "the real file should have landed");
        assert!(!destination.path().join("ghost.txt").exists(), "the absent one obviously did not");

        let mut report = SyncReport {
            generation: GEN,
            source: display(source.path()),
            destination: display(destination.path()),
            copied: 0,
            bytes_copied: 0,
            failures: Vec::new(),
        };
        copy_individually(source.path(), destination.path(), entries, &mut report);

        assert_eq!(report.copied, 1, "only the file that actually landed may be counted");
        assert_eq!(report.bytes_copied, 4);
        assert_eq!(report.failures.len(), 1, "the skipped file must be reported, got {:?}", report.failures);
        assert_eq!(report.failures[0].relative_path, "ghost.txt");
    }

    /// The fallback must not re-send bytes the batch already delivered.
    ///
    /// `ditto` is not all-or-nothing: it can exit non-zero having written most
    /// of a tree, which is what a full disk or a mid-walk permission denial
    /// look like. Re-copying everything would make the error path the slowest
    /// path in the system — a 500 GB transfer failing at 400 GB would serially
    /// re-copy all 500 GB, one process per file.
    #[test]
    fn the_fallback_skips_what_already_landed() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "done.txt", b"already");
        write(destination.path(), "done.txt", b"already");
        write(source.path(), "todo.txt", b"missing");

        let done = planned("done.txt", 7, SyncReason::Missing);
        let landed = already_landed(&done.entry, &destination.path().join("done.txt"));
        assert!(landed, "a file already at the destination at the right size must be skipped");

        // ...but a file whose bytes differ at the same size must NOT be skipped,
        // because that is exactly what ContentDiffers means.
        let same_size_different_bytes = planned("done.txt", 7, SyncReason::ContentDiffers);
        assert!(
            !already_landed(&same_size_different_bytes.entry, &destination.path().join("done.txt")),
            "a size match proves nothing when only the contents differed"
        );
    }

    /// `apply` validates the paths itself, rather than inheriting the check
    /// from the plan that authorised it.
    ///
    /// The two used to share validation because `apply` called `plan`. It no
    /// longer does — one walk, not two — so this asserts the property that
    /// refactor could have quietly dropped. An `apply` that validated less than
    /// its plan would be a way in, and the failure must be `BadPath`, decided
    /// BEFORE the token is even considered.
    #[test]
    fn applying_refuses_a_bad_path_before_it_looks_at_the_token() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "f.txt", b"x");

        let keys = RandomState::new();
        let honest = request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        let token = plan(&keys, GEN, NOW, honest).expect("plan").token.expect("token");

        let relative = Path::new("relative/path");
        let bad = request(relative, destination.path(), CompareMode::Quick, OnDiffer::Skip);
        let error = apply(&keys, GEN, NOW, bad, &token).expect_err("a relative source must be refused");
        assert!(
            matches!(error, SyncError::BadPath { .. }),
            "expected BadPath before any token check, got {error:?}"
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
        let plan = plan(&keys, GEN, NOW, request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip))
        .expect("plan");
        let token = plan.token.clone().expect("a copyable plan must get a token");

        let report = apply(&keys, GEN, NOW, request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip), &token)
        .expect("apply");

        assert_eq!(report.copied, 1);
        assert!(report.failures.is_empty(), "got {:?}", report.failures);
        // The missing file arrived, directories and all.
        assert_eq!(
            fs::read(destination.path().join("deep/new.txt")).expect("read"),
            b"brand new"
        );
        // And nothing else moved.
        assert_eq!(fs::read(destination.path().join("keep.txt")).expect("read"), b"same");
        assert_eq!(
            fs::read(destination.path().join("theirs.txt")).expect("read"),
            b"do not touch"
        );
    }

    /// The whole point of copying with `ditto` rather than `rsync`: the
    /// destination is a faithful twin, not just the same bytes.
    ///
    /// This is the property `copy_planned` exists to keep while still copying a
    /// SUBSET, and it is the one a `--files-from` rsync silently gives up. The
    /// system `rsync` on macOS 15 is openrsync, which rejects `--xattrs` and
    /// `--acls` outright.
    #[test]
    fn a_planned_copy_carries_extended_attributes() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "deep/doc.txt", b"payload");

        let marked = source.path().join("deep/doc.txt");
        let set = Command::new("/usr/bin/xattr")
            .args(["-w", "user.rdirstat.probe", "sentinel"])
            .arg(&marked)
            .status()
            .expect("xattr");
        assert!(set.success(), "the fixture itself must carry the attribute");

        let keys = RandomState::new();
        let ask = request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        let plan = plan(&keys, GEN, NOW, ask).expect("plan");
        let token = plan.token.clone().expect("token");
        let report = apply(&keys, GEN, NOW, ask, &token).expect("apply");
        assert!(report.failures.is_empty(), "got {:?}", report.failures);

        let landed = Command::new("/usr/bin/xattr")
            .arg(destination.path().join("deep/doc.txt"))
            .output()
            .expect("xattr");
        let names = String::from_utf8_lossy(&landed.stdout);
        assert!(
            names.contains("user.rdirstat.probe"),
            "the copy dropped the extended attribute; got {names:?}"
        );
    }

    /// A tree whose files are all empty must still be copied.
    ///
    /// Guards the failure family filed as nato-b6h.1, where a size-driven
    /// batcher never tripped its threshold and so transferred nothing while
    /// exiting 0. Nothing here is size-driven, and this is what keeps it that
    /// way. Empty files are not exotic: lockfiles, `__init__.py`, `.gitkeep`.
    #[test]
    fn a_tree_of_only_zero_byte_files_is_still_copied() {
        let source = scratch();
        let destination = scratch();
        for name in ["a.lock", "pkg/__init__.py", "deep/nested/.gitkeep"] {
            write(source.path(), name, b"");
        }

        let keys = RandomState::new();
        let ask = request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        let plan = plan(&keys, GEN, NOW, ask).expect("plan");
        let token = plan.token.clone().expect("an all-empty tree is still a copyable plan");
        let report = apply(&keys, GEN, NOW, ask, &token).expect("apply");

        assert_eq!(report.copied, 3, "failures: {:?}", report.failures);
        for name in ["a.lock", "pkg/__init__.py", "deep/nested/.gitkeep"] {
            assert!(destination.path().join(name).exists(), "{name} never arrived");
        }
    }

    /// A newline in a filename must not lose the file.
    ///
    /// `mkbom -i` is line-delimited with no NUL form, so such a name cannot go
    /// into the bom — it takes the per-file path instead. Guards the failure
    /// family filed against `rgigasync` as nato-b6h.3, where the same shape
    /// split one path into two bogus ones and lost the file.
    #[test]
    fn a_name_containing_a_newline_still_arrives() {
        let source = scratch();
        let destination = scratch();
        write(source.path(), "we\nird.txt", b"awkward");
        write(source.path(), "plain.txt", b"ordinary");

        let keys = RandomState::new();
        let ask = request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip);
        let plan = plan(&keys, GEN, NOW, ask).expect("plan");
        let token = plan.token.clone().expect("token");
        let report = apply(&keys, GEN, NOW, ask, &token).expect("apply");

        assert_eq!(report.copied, 2, "failures: {:?}", report.failures);
        assert_eq!(fs::read(destination.path().join("we\nird.txt")).expect("read"), b"awkward");
        assert_eq!(fs::read(destination.path().join("plain.txt")).expect("read"), b"ordinary");
    }

    /// Every ancestor of every path, and the root, exactly once.
    ///
    /// `mkbom` rejects an entry whose parent it has not already seen, and it
    /// does so while still exiting 0 — so getting this wrong yields a bom that
    /// copies less than the plan and reports success.
    #[test]
    fn a_bom_list_names_every_directory_above_every_file() {
        let entries = vec![
            planned("deep/nested/file.txt", 1, SyncReason::Missing),
            planned("top.txt", 1, SyncReason::Missing),
        ];
        let lines: Vec<String> = bom_lines(&entries).into_iter().collect();
        assert_eq!(lines, vec![".", "./deep", "./deep/nested", "./deep/nested/file.txt", "./top.txt"]);
        // Sorted, so a parent always precedes its children — which is the
        // property `mkbom` actually requires.
        assert!(lines.windows(2).all(|pair| pair[0] < pair[1]));
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
        let plan = plan(&keys, GEN, NOW, request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip))
        .expect("plan");
        let token = plan.token.clone().expect("token");

        write(source.path(), "surprise.txt", b"appeared after review");

        let error = apply(&keys, GEN, NOW, request(source.path(), destination.path(), CompareMode::Quick, OnDiffer::Skip), &token)
        .expect_err("a changed source must invalidate the plan");
        assert!(matches!(error, SyncError::InvalidConfirmation), "got {error:?}");
    }
}
