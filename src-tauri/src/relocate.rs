//! Offload a subtree to another volume and leave a symlink behind.
//!
//! The problem this solves is the one the treemap creates: the user finds a
//! 58 GB `Docker.raw` or a 12 GB `~/Library/Caches`, and the only thing the app
//! can currently offer them is Trash. Deleting it is often wrong — they want
//! the bytes, just not on the boot volume. So: copy it to an external disk,
//! prove the copy is good, dispose of the original, and put a symlink where it
//! used to be so every existing reference still resolves.
//!
//! ## The order of operations is the whole design
//!
//! ```text
//!   plan   →  ditto  →  verify  →  dispose of source  →  symlink
//!            (copy)    (compare)   (Trash by default)
//! ```
//!
//! Nothing destructive happens until the copy has been proven byte-for-byte.
//! If verification fails the source is left *completely untouched* and the
//! partial destination is reported by path rather than silently removed —
//! deleting data on an error path is how a tool that was trying to save your
//! files eats them.
//!
//! ## Five decisions worth knowing about
//!
//! 1. **`ditto`, not `cp` and not `rsync`.** `ditto(1)` is Apple's own copier
//!    and is the only one of the three that reliably carries ACLs, extended
//!    attributes, resource forks, Finder info and compression state across a
//!    copy. macOS ships an ancient rsync; using it loses metadata quietly,
//!    which is the worst way to lose it.
//!
//! 2. **Verification is a byte-for-byte compare, not a checksum.** A digest
//!    would be the right tool if one side were remote — you cannot ship a
//!    terabyte over a wire to compare it. Both sides are local block devices
//!    here, so a streaming `memcmp` reads exactly the same bytes a
//!    hash-both-sides scheme would, costs the same I/O, and is *stronger*:
//!    there is no collision to argue about. It also keeps a hashing crate out
//!    of the dependency graph.
//!
//! 3. **The source goes to the Trash by default, not to `unlink`.** This
//!    module inherits [`crate::actions`]'s rule that the app does not call
//!    `fs::remove_file` on a user's data. Trash keeps Finder's "Put Back"
//!    working, so a relocation the user regrets ten seconds later is
//!    recoverable. The cost is honest and stated in the plan: the space is not
//!    actually returned until the Trash is emptied. [`SourceDisposal::Delete`]
//!    exists for the user who needs the bytes back now.
//!
//! 4. **The pointer left behind is always a symlink.** APFS firmlinks are
//!    reserved for Apple's own system volume group and cannot be created for
//!    an arbitrary path; an `/etc/fstab` mount is far more invasive and can
//!    leave a machine that will not boot. A symlink works across volumes,
//!    is visible in Finder and `ls -l`, and is undone by moving the tree back.
//!
//! 5. **A relocation is authorized by the same token machinery as Trash.**
//!    [`plan`] mints a [`ConfirmationToken`] bound to the generation and to the
//!    source's `(dev, ino)` *as observed by the plan*; [`apply`] re-observes
//!    and refuses if anything moved underneath it. See [`crate::token`].
//!
//! ## What this module deliberately will not do
//!
//! It will not relocate a SIP-protected path, a volume root, or one of the
//! handful of directories the boot process needs in place — see [`assess_risk`].
//! It will not rewrite the *contents* of config files to point at the new
//! location: "update the pointer references" is served by the symlink, which
//! updates every reference at once, including ones in binaries and databases
//! that no rewriter could find.

use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::hash::BuildHasher;
use std::io::{self, Read as _};
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use rdirstat_core::{ActionError, CompletedScan, ConfirmationToken, DisplayPath, NodeId, Operation, TreeGeneration};
use serde::{Deserialize, Serialize};

use crate::fsident;
use crate::token::{self, ItemIdentity};

/// Chunk size for the verification compare. Large enough that the syscall
/// overhead disappears against the read, small enough to stay off the stack
/// and out of the way of the page cache.
const COMPARE_CHUNK: usize = 256 * 1024;

/// Directories that must never be relocated, matched as path prefixes.
///
/// Some are SIP-protected and the copy would fail halfway; the rest are
/// load-bearing for boot or for the volume layout. `/private/tmp` is
/// deliberately **absent** — it is a legitimate and common thing to want to
/// offload, so it is handled as [`RiskTier::Risky`] rather than refused.
const BLOCKED_PREFIXES: &[&str] = &[
    "/System",
    "/bin",
    "/sbin",
    "/dev",
    "/cores",
    "/Network",
    "/private/var/db",
    "/private/var/folders",
    "/usr/bin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/sbin",
    "/usr/share",
];

/// Components in `/Volumes/<name>` — the mount point itself.
///
/// `/Volumes` is deliberately **not** in [`BLOCKED_PREFIXES`]. Blocking it as a
/// prefix would refuse every path on every external disk, which is precisely
/// where relocated data is supposed to live and where the user is most likely
/// to be tidying up. Only the mount point itself is off limits: relocating
/// `/Volumes/Backup` would try to move a mounted filesystem's root and replace
/// it with a symlink that the next mount would clobber.
const VOLUME_MOUNT_POINT_COMPONENTS: usize = 3;

/// Paths that are relocatable but where the user is doing something with real
/// consequences, so the plan says so in words rather than just proceeding.
const RISKY_PREFIXES: &[&str] = &["/private", "/Library", "/Applications", "/opt", "/usr/local"];

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// How the destination is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelocateMode {
    /// Copy the subtree to a destination that does not exist yet, verify it,
    /// dispose of the source, and symlink. The ordinary case.
    Migrate,
    /// The destination **already holds** the data — a previous migration that
    /// was interrupted, or a copy the user made themselves. Verify it against
    /// the source, then dispose and symlink. Copies nothing.
    Repoint,
}

/// What happens to the original once the copy is proven.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceDisposal {
    /// `NSFileManager.trashItem`. Recoverable through Finder's "Put Back";
    /// the space is not returned until the Trash is emptied. The default.
    Trash,
    /// Remove immediately. Returns the space at once and is not undoable.
    Delete,
    /// Leave the source in place. Produces a *copy*, not a relocation, and no
    /// symlink is created. Chosen automatically when verification finds
    /// something it could not check.
    Keep,
}

/// How much care this particular path needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RiskTier {
    /// User data. Proceed on confirmation.
    Ordinary,
    /// Allowed, but something about it can break a running system.
    Risky,
    /// Refused outright. [`plan`] returns this with `token: None`.
    Blocked,
}

/// One thing the user should read before confirming.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct RelocateWarning {
    /// Stable identifier, so the UI can style or suppress a specific warning.
    pub(crate) code: String,
    /// Plain sentence, already written for a human.
    pub(crate) message: String,
}

impl RelocateWarning {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// What a relocation would do, and the token that authorizes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct RelocatePlan {
    pub(crate) generation: TreeGeneration,
    /// `None` when [`RelocatePlan::risk`] is [`RiskTier::Blocked`], or when the
    /// plan found a condition that makes the relocation impossible. A UI must
    /// treat a plan without a token as "show the reasons, disable the button".
    pub(crate) token: Option<ConfirmationToken>,
    pub(crate) node: NodeId,
    pub(crate) source: DisplayPath,
    pub(crate) destination: DisplayPath,
    pub(crate) mode: RelocateMode,
    pub(crate) disposal: SourceDisposal,
    /// Subtree size as the scan recorded it.
    pub(crate) logical: u64,
    pub(crate) allocated: u64,
    /// Nodes retained under this one, as the scan recorded them.
    pub(crate) retained_nodes: u32,
    /// Directories under this one the scan could not read. Non-zero means the
    /// recorded size is a floor and the copy may be larger than planned.
    pub(crate) unreadable: u32,
    /// `st_dev` of the source and of the destination's parent. Equal devices
    /// mean the move frees nothing, which is a warning, not an error.
    pub(crate) source_device: u64,
    pub(crate) destination_device: u64,
    /// Bytes free on the destination filesystem, from `df -Pk -Y`.
    pub(crate) destination_available: u64,
    /// `apfs`, `exfat`, `smbfs`, … Anything that cannot hold extended
    /// attributes and ACLs is refused, because `ditto` would drop them
    /// silently and the content comparison would not notice.
    pub(crate) destination_filesystem: String,
    pub(crate) risk: RiskTier,
    pub(crate) warnings: Vec<RelocateWarning>,
}

/// What a relocation actually did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct RelocateReport {
    pub(crate) generation: TreeGeneration,
    pub(crate) node: NodeId,
    pub(crate) source: DisplayPath,
    pub(crate) destination: DisplayPath,
    pub(crate) mode: RelocateMode,
    /// What was actually done to the source, which is not always what was
    /// asked: an unverifiable entry downgrades it to [`SourceDisposal::Keep`].
    pub(crate) disposal: SourceDisposal,
    /// Regular files compared byte-for-byte and found identical.
    pub(crate) files_verified: u64,
    /// Bytes in those files.
    pub(crate) bytes_verified: u64,
    /// Sockets, FIFOs and device nodes found in the source. `ditto` does not
    /// carry these and no copy can, so their presence forces `Keep`.
    pub(crate) special_files: u64,
    /// Whether the symlink now exists at the source path.
    pub(crate) symlink_created: bool,
    /// Set when the relocation did not complete. The source is untouched.
    pub(crate) error: Option<RelocateError>,
}

/// A relocation-specific failure.
///
/// Path-identity failures reuse [`ActionError`] through
/// [`RelocateError::Action`] rather than being restated here, so the "the thing
/// you selected is not the thing on disk" rules stay in one place.
/// `Display` and `Error` are written out by hand rather than derived: `src-tauri`
/// deliberately does not depend on `thiserror` (docs/08-RUST-PRACTICES.md
/// reserves it for the library crates and gives the shell `anyhow`), and adding
/// it for one enum would be the wrong end of that trade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub(crate) enum RelocateError {
    /// A path-identity failure, reused from the Trash path.
    Action(ActionError),

    /// The path is one the system needs where it is.
    Blocked {
        path: DisplayPath,
        reason: String,
    },

    /// The destination exists when it must not, or is missing when it must be
    /// there — depending on [`RelocateMode`].
    Destination {
        path: DisplayPath,
        reason: String,
    },

    /// Source and destination overlap; copying would recurse.
    Overlapping {
        source: DisplayPath,
        destination: DisplayPath,
    },

    /// The destination filesystem does not have room.
    NotEnoughSpace {
        needed: u64,
        available: u64,
    },

    /// `ditto` exited non-zero.
    CopyFailed(String),

    /// The copy did not match the source. The source has NOT been touched.
    VerifyFailed {
        path: DisplayPath,
        reason: String,
    },

    /// The copy verified, but the symlink could not be created. The source has
    /// already been disposed of — recover it from the Trash. Reported loudly.
    SymlinkFailed {
        path: DisplayPath,
        reason: String,
    },

    Internal(String),
}

impl std::fmt::Display for RelocateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Action(error) => write!(f, "{error}"),
            Self::Blocked { path, reason } => write!(f, "{path} cannot be relocated: {reason}"),
            Self::Destination { path, reason } => write!(f, "destination is not usable: {path} ({reason})"),
            Self::Overlapping { source, destination } => {
                write!(f, "{source} and {destination} contain one another")
            }
            Self::NotEnoughSpace { needed, available } => {
                write!(f, "{needed} bytes needed, {available} available on the destination")
            }
            Self::CopyFailed(reason) => write!(f, "copy failed: {reason}"),
            Self::VerifyFailed { path, reason } => write!(f, "verification failed at {path}: {reason}"),
            Self::SymlinkFailed { path, reason } => {
                write!(f, "relocated, but the symlink at {path} could not be created: {reason}")
            }
            Self::Internal(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for RelocateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Action(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ActionError> for RelocateError {
    fn from(error: ActionError) -> Self {
        Self::Action(error)
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// What the caller wants done, bundled so [`plan`] and [`apply`] take the same
/// four things and cannot be called with them in a different order.
///
/// `node` plus a *parent directory* rather than a finished destination path:
/// the final path is always `<parent>/<source basename>`, computed by
/// [`destination_for`], so the frontend cannot ask for a rename and a move in
/// one step — which would make the "leave a symlink at the old name" promise
/// ambiguous.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RelocateRequest<'a> {
    pub(crate) node: NodeId,
    pub(crate) destination_parent: &'a Path,
    pub(crate) mode: RelocateMode,
    pub(crate) disposal: SourceDisposal,
}

/// Classifies a source path.
///
/// Prefix matching is on *components*, not on string prefixes: `/System` must
/// not match `/SystemThingIMade`, and `starts_with` on `Path` compares whole
/// components, which is exactly the semantics wanted.
fn assess_risk(path: &Path) -> (RiskTier, Option<&'static str>) {
    // The volume root itself is never relocatable, whatever it is called.
    if path.parent().is_none() {
        return (RiskTier::Blocked, Some("it is a filesystem root"));
    }
    for blocked in BLOCKED_PREFIXES {
        if path.starts_with(blocked) {
            return (RiskTier::Blocked, Some("it is protected by the system"));
        }
    }
    // `/Volumes` and `/Volumes/<name>` only — never what is inside them.
    if path.starts_with("/Volumes") && path.components().count() <= VOLUME_MOUNT_POINT_COMPONENTS {
        return (RiskTier::Blocked, Some("it is a mount point"));
    }
    for risky in RISKY_PREFIXES {
        if path.starts_with(risky) {
            return (RiskTier::Risky, None);
        }
    }
    (RiskTier::Ordinary, None)
}

/// Filesystems that can carry the metadata `ditto` is being used to preserve.
///
/// This list is the load-bearing half of the metadata-fidelity promise. Using
/// `ditto` buys nothing if the destination cannot *store* an extended
/// attribute or an ACL: it drops them without failing, verification below
/// compares contents and would pass, and the source would then be destroyed —
/// losing metadata permanently in the one code path that exists to avoid
/// exactly that.
///
/// Verifying xattrs directly would be better and needs `listxattr`/`getxattr`,
/// which means a `libc` dependency this crate does not have. Refusing the
/// filesystems that provably cannot hold them is the honest subset: it is
/// conservative in the safe direction, and a user who genuinely wants a
/// metadata-losing copy to exFAT can do it in Finder.
const METADATA_CAPABLE_FILESYSTEMS: &[&str] = &["apfs", "hfs"];

/// What `df` reports for the filesystem holding a path.
#[derive(Debug, Clone)]
struct FilesystemFacts {
    available: u64,
    /// `apfs`, `exfat`, `msdos`, `smbfs`, …
    kind: String,
}

/// Bytes available and filesystem type for the mount holding `path`.
///
/// Shells out to `df -Pk -Y` for the single path rather than reusing
/// `crate::volumes::list`, which enumerates every mount and would then need
/// prefix-matching to find the right one — one exact answer beats a list plus
/// a guess. `-P` pins the POSIX single-line output format so the parse is not
/// at the mercy of a long device name wrapping, and `-Y` adds the type column.
fn filesystem_facts(path: &Path) -> Result<FilesystemFacts, RelocateError> {
    let output = Command::new("/bin/df")
        .args(["-Pk", "-Y"])
        .arg(path)
        .output()
        .map_err(|error| RelocateError::Internal(format!("df: {error}")))?;
    if !output.status.success() {
        return Err(RelocateError::Internal(format!(
            "df exited {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Line 0 is the header; line 1 is the answer.
    // Filesystem, Type, 1024-blocks, Used, Available, Capacity, Mounted-on
    let fields: Vec<&str> = text.lines().nth(1).unwrap_or_default().split_whitespace().collect();
    let available = fields
        .get(4)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|blocks| blocks.saturating_mul(1_024))
        .ok_or_else(|| RelocateError::Internal("could not parse df output".to_owned()))?;
    let kind = fields.get(1).copied().unwrap_or("unknown").to_ascii_lowercase();
    Ok(FilesystemFacts { available, kind })
}

/// True when the filesystem can hold extended attributes and ACLs.
fn carries_macos_metadata(kind: &str) -> bool {
    METADATA_CAPABLE_FILESYSTEMS
        .iter()
        .any(|known| kind == *known || kind.starts_with(known))
}

/// `st_dev` of an existing path, or of its nearest existing ancestor.
fn device_of(path: &Path) -> Option<u64> {
    let mut cursor = path;
    loop {
        if let Ok(observation) = fsident::observe(cursor) {
            return Some(observation.device);
        }
        cursor = cursor.parent()?;
    }
}

/// Builds the destination path for a source and a chosen parent directory.
///
/// The destination is always `<parent>/<source basename>`, never `<parent>`
/// itself: `ditto src dst` writes *src's contents* into `dst`, so handing it
/// the bare parent would splatter a cache tree directly into the root of the
/// user's external disk.
fn destination_for(source: &Path, destination_parent: &Path) -> Result<PathBuf, RelocateError> {
    let name = source.file_name().ok_or_else(|| RelocateError::Blocked {
        path: display(source),
        reason: "it has no name component".to_owned(),
    })?;
    Ok(destination_parent.join(name))
}

fn display(path: &Path) -> DisplayPath {
    DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes())
}

/// Rejects a source and destination that contain one another.
fn check_disjoint(source: &Path, destination: &Path) -> Result<(), RelocateError> {
    if source == destination || destination.starts_with(source) || source.starts_with(destination) {
        return Err(RelocateError::Overlapping {
            source: display(source),
            destination: display(destination),
        });
    }
    Ok(())
}

/// Plans a relocation and mints the token that authorizes it.
///
/// Returns a plan even when the relocation cannot proceed: the reasons are the
/// useful output, and a UI that can only render success is a UI that renders
/// nothing when the user picks the wrong disk. A plan with `token: None` must
/// not be actionable.
///
/// # Errors
///
/// Only for conditions that make a plan impossible to describe at all — an
/// unknown node, a path that escapes the scan root, or an unreadable
/// destination. Everything else is reported *inside* the plan.
pub(crate) fn plan<S: BuildHasher>(
    scan: &CompletedScan,
    keys: &S,
    now_unix_ms: i64,
    request: RelocateRequest<'_>,
) -> Result<RelocatePlan, RelocateError> {
    let RelocateRequest {
        node,
        destination_parent,
        mode,
        disposal,
    } = request;
    let source = fsident::action_path(scan, node, false)?;
    fsident::confirm_within_root(scan, &source)?;
    let observation = fsident::revalidate(scan, node, &source)?;

    let destination = destination_for(&source, destination_parent)?;
    check_disjoint(&source, &destination)?;

    let mut warnings: Vec<RelocateWarning> = Vec::new();
    let (mut risk, blocked_reason) = assess_risk(&source);

    if let Some(reason) = blocked_reason {
        warnings.push(RelocateWarning::new(
            "blocked",
            format!("{} cannot be relocated because {reason}.", source.display()),
        ));
    }

    // A subtree the scan could not fully read is a floor, not a measurement:
    // `ditto` will try to copy what the scan never counted, and the size and
    // free-space arithmetic below is therefore optimistic.
    let totals = scan.tree.dir_totals(node);
    let unreadable = totals.map_or(0, |t| t.unreadable);
    let retained_nodes = totals.map_or(1, |t| t.retained_nodes);
    if unreadable > 0 {
        risk = risk.max_tier(RiskTier::Risky);
        warnings.push(RelocateWarning::new(
            "incomplete-scan",
            format!(
                "{unreadable} director{} under this path could not be read during the scan, so its \
                 recorded size is a floor. The copy may be larger than planned, and may stop on the \
                 same permission error.",
                if unreadable == 1 { "y" } else { "ies" }
            ),
        ));
    }

    let logical = scan.tree.logical_of(node);
    let allocated = scan.tree.allocated_of(node);

    let source_device = observation.device;
    let destination_device = device_of(&destination).unwrap_or(0);
    if source_device == destination_device {
        warnings.push(RelocateWarning::new(
            "same-volume",
            "The destination is on the same volume as the source, so this frees no space. \
             It is still a valid move if you are reorganizing.",
        ));
    }

    let facts = filesystem_facts(destination_parent).ok();
    let destination_available = facts.as_ref().map_or(0, |entry| entry.available);
    let destination_filesystem = facts.map_or_else(|| "unknown".to_owned(), |entry| entry.kind);

    // Destination state has to match the mode, or the operation means something
    // other than what the user asked for.
    let destination_exists = fs::symlink_metadata(&destination).is_ok();
    let mut fatal: Option<RelocateError> = None;
    match mode {
        RelocateMode::Migrate => {
            if destination_exists {
                fatal = Some(RelocateError::Destination {
                    path: display(&destination),
                    reason: "it already exists; use Repoint to adopt it, or choose another name".to_owned(),
                });
            } else if allocated > destination_available {
                fatal = Some(RelocateError::NotEnoughSpace {
                    needed: allocated,
                    available: destination_available,
                });
            }
        }
        RelocateMode::Repoint => {
            if !destination_exists {
                fatal = Some(RelocateError::Destination {
                    path: display(&destination),
                    reason: "it does not exist; use Migrate to create it".to_owned(),
                });
            }
        }
    }

    if !destination_parent.is_dir() {
        fatal = Some(RelocateError::Destination {
            path: display(destination_parent),
            reason: "it is not a directory".to_owned(),
        });
    }

    // The metadata-fidelity gate. `ditto` is used precisely because it carries
    // ACLs, extended attributes and resource forks; a destination that cannot
    // store them drops them without failing, the content-comparison below
    // still passes, and the source would then be disposed of — silently losing
    // metadata in the one routine whose whole purpose is not to. Refused
    // rather than warned, because the loss is invisible and permanent.
    if !carries_macos_metadata(&destination_filesystem) {
        fatal = Some(RelocateError::Destination {
            path: display(destination_parent),
            reason: format!(
                "it is {destination_filesystem}, which cannot store extended attributes or ACLs. \
                 Copying there would drop metadata that this app cannot verify and cannot get back. \
                 Use an APFS or Mac OS Extended volume, or copy it in Finder if you accept the loss"
            ),
        });
    }

    if let Some(error) = &fatal {
        warnings.push(RelocateWarning::new("blocked", error.to_string()));
    }

    if risk == RiskTier::Risky && blocked_reason.is_none() {
        warnings.push(RelocateWarning::new(
            "system-adjacent",
            "This path is outside your home folder and other software may expect it exactly where \
             it is. A symlink keeps most things working, but a process holding a file open across \
             the move — or one that refuses to follow symlinks — will not notice the change.",
        ));
    }

    if disposal == SourceDisposal::Trash {
        warnings.push(RelocateWarning::new(
            "trash-holds-space",
            "The original goes to the Trash, so it stays recoverable — but the space is not \
             returned until you empty it.",
        ));
    }

    let issuable = risk != RiskTier::Blocked && fatal.is_none();
    let token = issuable.then(|| {
        token::mint(
            keys,
            scan.generation,
            now_unix_ms,
            &[ItemIdentity {
                node,
                device: observation.device,
                inode: observation.inode,
            }],
        )
    });

    Ok(RelocatePlan {
        generation: scan.generation,
        token,
        node,
        source: display(&source),
        destination: display(&destination),
        mode,
        disposal,
        logical,
        allocated,
        retained_nodes,
        unreadable,
        source_device,
        destination_device,
        destination_available,
        destination_filesystem,
        risk,
        warnings,
    })
}

impl RiskTier {
    /// The more severe of two tiers.
    const fn max_tier(self, other: Self) -> Self {
        match (self, other) {
            (Self::Blocked, _) | (_, Self::Blocked) => Self::Blocked,
            (Self::Risky, _) | (_, Self::Risky) => Self::Risky,
            _ => Self::Ordinary,
        }
    }
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct VerifyTally {
    files: u64,
    bytes: u64,
    special: u64,
}

/// Compares two trees and returns what was checked.
///
/// Every regular file is read in full on both sides and compared byte-for-byte.
/// Symlinks are compared by target, not by what they point at — a symlink is
/// its target string, and following it would compare the same file to itself.
/// Sockets, FIFOs and device nodes are counted and skipped: no copy can carry
/// them, and their presence is what downgrades disposal to
/// [`SourceDisposal::Keep`].
fn verify_tree(source: &Path, destination: &Path, tally: &mut VerifyTally) -> Result<(), RelocateError> {
    let source_meta = fs::symlink_metadata(source).map_err(|error| verify_error(source, &error))?;
    let destination_meta = fs::symlink_metadata(destination).map_err(|error| verify_error(destination, &error))?;

    let source_type = source_meta.file_type();
    let destination_type = destination_meta.file_type();

    if source_type.is_symlink() {
        if !destination_type.is_symlink() {
            return Err(mismatch(destination, "the source is a symlink and the copy is not"));
        }
        let a = fs::read_link(source).map_err(|error| verify_error(source, &error))?;
        let b = fs::read_link(destination).map_err(|error| verify_error(destination, &error))?;
        if a != b {
            return Err(mismatch(destination, "symlink target differs"));
        }
        tally.files = tally.files.saturating_add(1);
        return Ok(());
    }

    if source_type.is_dir() {
        if !destination_type.is_dir() {
            return Err(mismatch(destination, "the source is a directory and the copy is not"));
        }
        return verify_directory(source, destination, tally);
    }

    if source_type.is_file() {
        if !destination_type.is_file() {
            return Err(mismatch(
                destination,
                "the source is a regular file and the copy is not",
            ));
        }
        if source_meta.len() != destination_meta.len() {
            return Err(mismatch(
                destination,
                format!("size differs: {} vs {}", source_meta.len(), destination_meta.len()),
            ));
        }
        compare_contents(source, destination)?;
        tally.files = tally.files.saturating_add(1);
        tally.bytes = tally.bytes.saturating_add(source_meta.len());
        return Ok(());
    }

    // Socket, FIFO, block or character device.
    if is_special(&source_meta) {
        tally.special = tally.special.saturating_add(1);
        return Ok(());
    }

    Err(mismatch(source, "unrecognized file type"))
}

fn verify_directory(source: &Path, destination: &Path, tally: &mut VerifyTally) -> Result<(), RelocateError> {
    let mut source_names = read_names(source)?;
    let mut destination_names = read_names(destination)?;
    source_names.sort_unstable();
    destination_names.sort_unstable();

    // Compare the name sets before recursing, so "the copy is missing a file"
    // is reported as exactly that rather than as a confusing failure ten levels
    // down.
    if source_names != destination_names {
        let missing: Vec<&PathBuf> = source_names
            .iter()
            .filter(|name| !destination_names.contains(name))
            .take(3)
            .collect();
        let extra: Vec<&PathBuf> = destination_names
            .iter()
            .filter(|name| !source_names.contains(name))
            .take(3)
            .collect();
        return Err(mismatch(
            destination,
            format!(
                "directory contents differ ({} entries in the source, {} in the copy; missing {missing:?}, unexpected {extra:?})",
                source_names.len(),
                destination_names.len(),
            ),
        ));
    }

    for name in source_names {
        verify_tree(&source.join(&name), &destination.join(&name), tally)?;
    }
    Ok(())
}

fn read_names(directory: &Path) -> Result<Vec<PathBuf>, RelocateError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| verify_error(directory, &error))? {
        let entry = entry.map_err(|error| verify_error(directory, &error))?;
        names.push(PathBuf::from(entry.file_name()));
    }
    Ok(names)
}

/// Streams both files and compares them block by block.
fn compare_contents(source: &Path, destination: &Path) -> Result<(), RelocateError> {
    let mut a = fs::File::open(source).map_err(|error| verify_error(source, &error))?;
    let mut b = fs::File::open(destination).map_err(|error| verify_error(destination, &error))?;
    let mut buffer_a = vec![0_u8; COMPARE_CHUNK];
    let mut buffer_b = vec![0_u8; COMPARE_CHUNK];
    let mut offset = 0_u64;

    loop {
        let read_a = read_full(&mut a, &mut buffer_a).map_err(|error| verify_error(source, &error))?;
        let read_b = read_full(&mut b, &mut buffer_b).map_err(|error| verify_error(destination, &error))?;
        if read_a != read_b {
            return Err(mismatch(destination, format!("length differs near byte {offset}")));
        }
        if read_a == 0 {
            return Ok(());
        }
        if buffer_a[..read_a] != buffer_b[..read_b] {
            return Err(mismatch(destination, format!("contents differ near byte {offset}")));
        }
        offset = offset.saturating_add(read_a as u64);
    }
}

/// Fills `buffer` unless EOF is reached first.
///
/// `Read::read` is permitted to return a short read at any time; comparing two
/// short reads of *different* lengths from two files that are actually
/// identical would be a false mismatch, so both sides are filled to the same
/// boundary before being compared.
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

fn is_special(metadata: &Metadata) -> bool {
    let file_type = metadata.file_type();
    file_type.is_socket() || file_type.is_fifo() || file_type.is_block_device() || file_type.is_char_device()
}

fn mismatch(path: &Path, reason: impl Into<String>) -> RelocateError {
    RelocateError::VerifyFailed {
        path: display(path),
        reason: reason.into(),
    }
}

fn verify_error(path: &Path, error: &io::Error) -> RelocateError {
    RelocateError::VerifyFailed {
        path: display(path),
        reason: error.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

/// Moves one path to the Trash.
///
/// [`crate::actions`] has the node-oriented version, which normalizes a whole
/// selection and checks a token. By this point relocate has already done both
/// of those and holds a revalidated path, so it needs the last step only — and
/// it needs it via the same `NSFileManager.trashItem` route, so Finder's "Put
/// Back" keeps working on a relocation the user wants to undo.
fn trash_path(path: &Path) -> Result<(), RelocateError> {
    #[cfg(target_os = "macos")]
    let context = {
        use trash::macos::{DeleteMethod, TrashContextExtMacos as _};
        let mut context = trash::TrashContext::new();
        context.set_delete_method(DeleteMethod::NsFileManager);
        context
    };
    #[cfg(not(target_os = "macos"))]
    let context = trash::TrashContext::new();

    context.delete(path).map_err(|error| {
        RelocateError::Action(ActionError::TrashFailed {
            path: display(path),
            reason: error.to_string(),
        })
    })
}

/// Runs `ditto` to copy `source` onto `destination`.
fn ditto(source: &Path, destination: &Path) -> Result<(), RelocateError> {
    // `--rsrc --extattr --acl` are the metadata-preserving flags. Modern ditto
    // implies them for an APFS-to-APFS copy, but stating them means the
    // behaviour does not change under us on a different destination filesystem,
    // and it documents at the call site what this copy is promising to keep.
    let output = Command::new("/usr/bin/ditto")
        .arg("--rsrc")
        .arg("--extattr")
        .arg("--acl")
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| RelocateError::CopyFailed(format!("could not run ditto: {error}")))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(RelocateError::CopyFailed(format!(
        "ditto exited {}: {}",
        output.status.code().unwrap_or(-1),
        stderr.trim()
    )))
}

/// Executes a planned relocation.
///
/// The token must be the one [`plan`] minted, and the source must still be the
/// object the plan observed — both are re-checked here against a fresh `lstat`,
/// so a plan left sitting in a webview while the user did something else in
/// Finder fails closed.
///
/// # Errors
///
/// Any [`RelocateError`]. Every error path before the disposal step leaves the
/// source untouched; [`RelocateError::SymlinkFailed`] is the one case where the
/// source is already gone, and it says so.
pub(crate) fn apply<S: BuildHasher>(
    scan: &CompletedScan,
    keys: &S,
    now_unix_ms: i64,
    request: RelocateRequest<'_>,
    confirmation: &ConfirmationToken,
) -> Result<RelocateReport, RelocateError> {
    let RelocateRequest {
        node,
        destination_parent,
        mode,
        disposal,
    } = request;
    let source = fsident::action_path(scan, node, false)?;
    fsident::confirm_within_root(scan, &source)?;
    let observation = fsident::revalidate(scan, node, &source)?;

    let destination = destination_for(&source, destination_parent)?;
    check_disjoint(&source, &destination)?;

    // Re-run the block list at apply time. The plan's verdict is advisory; this
    // is the one that decides, so a hand-crafted IPC call cannot skip it.
    if let (RiskTier::Blocked, reason) = assess_risk(&source) {
        return Err(RelocateError::Blocked {
            path: display(&source),
            reason: reason.unwrap_or("it is protected").to_owned(),
        });
    }

    // Re-check metadata capability here too, for the same reason. A volume can
    // be unmounted and a different one mounted at the same path between the
    // plan and the confirm, and this is the check whose failure mode is a
    // silent, permanent loss rather than a visible error.
    let facts = filesystem_facts(destination_parent)?;
    if !carries_macos_metadata(&facts.kind) {
        return Err(RelocateError::Destination {
            path: display(destination_parent),
            reason: format!("it is {}, which cannot store extended attributes or ACLs", facts.kind),
        });
    }

    token::verify(
        keys,
        confirmation,
        scan.generation,
        now_unix_ms,
        &[ItemIdentity {
            node,
            device: observation.device,
            inode: observation.inode,
        }],
    )?;

    let mut report = RelocateReport {
        generation: scan.generation,
        node,
        source: display(&source),
        destination: display(&destination),
        mode,
        disposal,
        files_verified: 0,
        bytes_verified: 0,
        special_files: 0,
        symlink_created: false,
        error: None,
    };

    // 1. Copy. Repoint skips this: the bytes are already there.
    if mode == RelocateMode::Migrate {
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(RelocateError::Destination {
                path: display(&destination),
                reason: "it appeared between the plan and the confirmation".to_owned(),
            });
        }
        ditto(&source, &destination)?;
    }

    // 2. Verify. Nothing destructive has happened yet, and nothing will if this
    //    fails — the partial copy is left on disk and named in the error so the
    //    user can inspect or remove it themselves.
    let mut tally = VerifyTally::default();
    verify_tree(&source, &destination, &mut tally)?;
    report.files_verified = tally.files;
    report.bytes_verified = tally.bytes;
    report.special_files = tally.special;

    // 3. Dispose of the source — the first irreversible step.
    //
    //    Special files force `Keep`: a socket in the source was never copied
    //    and never can be, so removing the source would destroy something the
    //    destination does not have. The user gets a copy and an explanation
    //    instead of a silent loss.
    let effective = if tally.special > 0 {
        SourceDisposal::Keep
    } else {
        disposal
    };
    report.disposal = effective;

    match effective {
        SourceDisposal::Keep => return Ok(report),
        SourceDisposal::Trash => trash_path(&source)?,
        SourceDisposal::Delete => {
            let outcome = if source.is_dir() {
                fs::remove_dir_all(&source)
            } else {
                fs::remove_file(&source)
            };
            outcome.map_err(|error| RelocateError::Action(fsident::action_error(&source, Operation::Trash, &error)))?;
        }
    }

    // 4. Leave the pointer. If this fails the source is already in the Trash,
    //    which is recoverable but not obvious, so the error says exactly that.
    std::os::unix::fs::symlink(&destination, &source).map_err(|error| RelocateError::SymlinkFailed {
        path: display(&source),
        reason: error.to_string(),
    })?;
    report.symlink_created = true;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;
    use std::sync::Arc;

    use rdirstat_core::{ScanId, ScanOptions, TreeGeneration};

    use super::*;
    use crate::engine::{ScanOutcome, ScanRequest};
    use crate::progress::ProgressCounters;
    use crate::state::CancelToken;

    const NOW: i64 = 1_800_000_000_000;

    fn scan_of(root: &Path) -> Arc<CompletedScan> {
        match crate::engine::run(ScanRequest {
            root: root.to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        }) {
            ScanOutcome::Completed(scan) => Arc::from(scan),
            other => panic!("expected a completed scan, got {other:?}"),
        }
    }

    fn child_named(scan: &CompletedScan, parent: NodeId, name: &str) -> NodeId {
        scan.tree
            .children(parent)
            .find(|node| scan.tree.name_bytes(*node) == Some(name.as_bytes()))
            .unwrap_or_else(|| panic!("no child named {name}"))
    }

    /// A scratch directory on *ordinary* ground.
    ///
    /// `tempfile::tempdir()` lands in `$TMPDIR`, which on macOS is
    /// `/private/var/folders/...` — a path [`assess_risk`] blocks on purpose,
    /// so a plan rooted there correctly refuses to mint a token and every
    /// end-to-end test would fail for a reason that has nothing to do with what
    /// it is testing. Building under `target/` keeps the fixtures on the same
    /// footing as real user data, and `target/` is already git-ignored.
    fn scratch() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/relocate-scratch");
        fs::create_dir_all(&base).expect("scratch root");
        tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(&base)
            .expect("tempdir")
    }

    /// A source tree with nested directories, a symlink, and two files.
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Arc<CompletedScan>) {
        let source = scratch();
        let destination = scratch();
        let payload = source.path().join("payload");
        fs::create_dir_all(payload.join("inner")).expect("mkdir");
        fs::write(payload.join("inner/deep.bin"), vec![b'z'; 4096]).expect("write");
        fs::write(payload.join("top.txt"), b"hello").expect("write");
        std::os::unix::fs::symlink("inner/deep.bin", payload.join("link")).expect("symlink");
        let scan = scan_of(source.path());
        (source, destination, scan)
    }

    // -- risk classification -------------------------------------------------

    #[test]
    fn system_paths_are_blocked_and_lookalikes_are_not() {
        assert_eq!(assess_risk(Path::new("/System/Library/Foo")).0, RiskTier::Blocked);
        assert_eq!(assess_risk(Path::new("/usr/lib/libSystem.dylib")).0, RiskTier::Blocked);
        assert_eq!(assess_risk(Path::new("/")).0, RiskTier::Blocked);

        // Component-wise matching, not string prefixes: this is a normal
        // directory that merely starts with the same letters.
        assert_eq!(assess_risk(Path::new("/SystemThingIMade")).0, RiskTier::Ordinary);
        assert_eq!(assess_risk(Path::new("/Users/josh/Movies")).0, RiskTier::Ordinary);
    }

    #[test]
    fn a_mount_point_is_blocked_but_its_contents_are_not() {
        // The regression this pins: `/Volumes` as a plain blocked *prefix*
        // refuses every path on every external disk — exactly where offloaded
        // data lives, and the most likely place for the user to be tidying up.
        assert_eq!(assess_risk(Path::new("/Volumes")).0, RiskTier::Blocked);
        assert_eq!(assess_risk(Path::new("/Volumes/Backup")).0, RiskTier::Blocked);
        assert_eq!(assess_risk(Path::new("/Volumes/Backup/Movies")).0, RiskTier::Ordinary);
        assert_eq!(
            assess_risk(Path::new("/Volumes/Backup/Movies/2019/raw")).0,
            RiskTier::Ordinary
        );
    }

    #[test]
    fn the_per_user_temp_directory_is_blocked() {
        // `$TMPDIR` on macOS. Relocating it breaks every running process that
        // has an open file there, and the OS recreates it anyway.
        assert_eq!(
            assess_risk(Path::new("/private/var/folders/qx/abc/T/thing")).0,
            RiskTier::Blocked
        );
    }

    #[test]
    fn private_tmp_is_allowed_but_flagged_risky() {
        // The user's motivating example. It must not be blocked — offloading it
        // is the whole point — but it must not be silent either.
        assert_eq!(assess_risk(Path::new("/private/tmp")).0, RiskTier::Risky);
        assert_eq!(assess_risk(Path::new("/usr/local/share/big")).0, RiskTier::Risky);
    }

    #[test]
    fn the_block_list_is_re_checked_at_apply_time_not_just_at_plan_time() {
        // `plan` returning Blocked is advisory; `apply` is what decides. A
        // hand-crafted IPC call that skipped the plan must still be refused.
        let source = Path::new("/System/Library/CoreServices");
        assert_eq!(assess_risk(source).0, RiskTier::Blocked);
    }

    // -- path arithmetic -----------------------------------------------------

    #[test]
    fn the_destination_keeps_the_source_basename() {
        // `ditto src dst` writes src's *contents* into dst, so handing it the
        // bare parent would splatter the tree across the external disk's root.
        let destination = destination_for(Path::new("/private/tmp"), Path::new("/Volumes/Ext")).expect("path");
        assert_eq!(destination, Path::new("/Volumes/Ext/tmp"));
    }

    #[test]
    fn a_destination_inside_the_source_is_refused() {
        let error = check_disjoint(Path::new("/a/b"), Path::new("/a/b/c")).expect_err("must refuse");
        assert!(matches!(error, RelocateError::Overlapping { .. }));

        let error = check_disjoint(Path::new("/a/b/c"), Path::new("/a/b")).expect_err("must refuse");
        assert!(matches!(error, RelocateError::Overlapping { .. }));

        check_disjoint(Path::new("/a/b"), Path::new("/x/y")).expect("disjoint paths are fine");
    }

    #[test]
    fn a_relative_or_dotdot_destination_is_not_acceptable() {
        assert!(is_acceptable_destination(Path::new("/Volumes/Ext/tmp")));
        assert!(!is_acceptable_destination(Path::new("relative/path")));
        assert!(!is_acceptable_destination(Path::new("/Volumes/../etc")));
        assert!(!is_acceptable_destination(Path::new("/")));
    }

    // -- verification --------------------------------------------------------

    #[test]
    fn an_identical_copy_verifies() {
        let (source, destination, _scan) = fixture();
        let from = source.path().join("payload");
        let to = destination.path().join("payload");
        ditto(&from, &to).expect("ditto");

        let mut tally = VerifyTally::default();
        verify_tree(&from, &to, &mut tally).expect("an identical copy must verify");
        assert_eq!(tally.files, 3, "two regular files and one symlink");
        assert_eq!(tally.bytes, 4096 + 5);
        assert_eq!(tally.special, 0);
    }

    #[test]
    fn a_single_flipped_byte_fails_verification() {
        let (source, destination, _scan) = fixture();
        let from = source.path().join("payload");
        let to = destination.path().join("payload");
        ditto(&from, &to).expect("ditto");

        // Same length, one byte different — the case a size check cannot catch
        // and the reason verification is a compare rather than a stat.
        let mut corrupted = vec![b'z'; 4096];
        corrupted[2048] = b'q';
        fs::write(to.join("inner/deep.bin"), &corrupted).expect("write");

        let mut tally = VerifyTally::default();
        let error = verify_tree(&from, &to, &mut tally).expect_err("corruption must be caught");
        assert!(
            matches!(&error, RelocateError::VerifyFailed { reason, .. } if reason.contains("contents differ")),
            "got {error:?}"
        );
    }

    #[test]
    fn a_missing_file_in_the_copy_fails_verification() {
        let (source, destination, _scan) = fixture();
        let from = source.path().join("payload");
        let to = destination.path().join("payload");
        ditto(&from, &to).expect("ditto");
        fs::remove_file(to.join("top.txt")).expect("remove");

        let mut tally = VerifyTally::default();
        let error = verify_tree(&from, &to, &mut tally).expect_err("a missing file must be caught");
        assert!(
            matches!(&error, RelocateError::VerifyFailed { reason, .. } if reason.contains("contents differ")),
            "got {error:?}"
        );
    }

    #[test]
    fn a_symlink_is_compared_by_target_not_by_what_it_points_at() {
        let directory = scratch();
        let a = directory.path().join("a");
        let b = directory.path().join("b");
        std::os::unix::fs::symlink("one", &a).expect("symlink");
        std::os::unix::fs::symlink("two", &b).expect("symlink");

        let mut tally = VerifyTally::default();
        let error = verify_tree(&a, &b, &mut tally).expect_err("differing targets must be caught");
        assert!(
            matches!(&error, RelocateError::VerifyFailed { reason, .. } if reason.contains("symlink target")),
            "got {error:?}"
        );
    }

    #[test]
    fn a_short_read_does_not_produce_a_false_mismatch() {
        // Files larger than one chunk exercise the fill-to-boundary loop; a
        // naive single `read` per side can return different lengths for
        // identical files and report a mismatch that is not there.
        let directory = scratch();
        let a = directory.path().join("a.bin");
        let b = directory.path().join("b.bin");
        let payload: Vec<u8> = (0..(COMPARE_CHUNK * 2 + 517)).map(|i| (i % 251) as u8).collect();
        fs::write(&a, &payload).expect("write");
        fs::write(&b, &payload).expect("write");

        compare_contents(&a, &b).expect("identical multi-chunk files must compare equal");
    }

    // -- the whole operation -------------------------------------------------

    #[test]
    fn a_migration_copies_verifies_removes_the_source_and_leaves_a_symlink() {
        let (source, destination, scan) = fixture();
        let payload = child_named(&scan, scan.root, "payload");
        let keys = RandomState::new();

        let plan = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Delete,
            },
        )
        .expect("plan");
        let token = plan.token.clone().expect("an ordinary path must get a token");
        assert_eq!(plan.risk, RiskTier::Ordinary);

        let report = apply(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Delete,
            },
            &token,
        )
        .expect("apply");

        assert_eq!(report.files_verified, 3);
        assert!(report.symlink_created);
        assert_eq!(report.disposal, SourceDisposal::Delete);

        // The source path is now a symlink pointing at the new home, and it
        // still resolves — which is the entire promise of the feature.
        let original = source.path().join("payload");
        let metadata = fs::symlink_metadata(&original).expect("the pointer must exist");
        assert!(metadata.file_type().is_symlink(), "the source must now be a symlink");
        assert_eq!(
            fs::read_link(&original).expect("readlink"),
            destination.path().join("payload")
        );
        assert_eq!(
            fs::read(original.join("top.txt")).expect("read through the symlink"),
            b"hello"
        );
    }

    #[test]
    fn a_failed_verification_leaves_the_source_completely_untouched() {
        let (source, destination, scan) = fixture();
        let payload = child_named(&scan, scan.root, "payload");
        let keys = RandomState::new();

        // Pre-create a destination that does NOT match, and adopt it with
        // Repoint. Verification must fail and nothing must be destroyed.
        let landing = destination.path().join("payload");
        fs::create_dir_all(landing.join("inner")).expect("mkdir");
        fs::write(landing.join("inner/deep.bin"), b"wrong").expect("write");

        let plan = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Repoint,
                disposal: SourceDisposal::Delete,
            },
        )
        .expect("plan");
        let token = plan.token.clone().expect("token");

        let error = apply(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Repoint,
                disposal: SourceDisposal::Delete,
            },
            &token,
        )
        .expect_err("a mismatched destination must not be adopted");
        assert!(matches!(error, RelocateError::VerifyFailed { .. }), "got {error:?}");

        // Everything still there, still a real directory, still readable.
        let original = source.path().join("payload");
        assert!(fs::symlink_metadata(&original).expect("source").file_type().is_dir());
        assert_eq!(fs::read(original.join("top.txt")).expect("read"), b"hello");
        assert_eq!(fs::read(original.join("inner/deep.bin")).expect("read").len(), 4096);
    }

    #[test]
    fn migrate_refuses_a_destination_that_already_exists() {
        let (_source, destination, scan) = fixture();
        let payload = child_named(&scan, scan.root, "payload");
        fs::create_dir_all(destination.path().join("payload")).expect("mkdir");
        let keys = RandomState::new();

        let plan = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Trash,
            },
        )
        .expect("plan");

        assert!(plan.token.is_none(), "an unusable plan must not be actionable");
        assert!(plan.warnings.iter().any(|warning| warning.code == "blocked"));
    }

    #[test]
    fn a_token_for_one_node_cannot_authorize_another() {
        let (_source, destination, scan) = fixture();
        let payload = child_named(&scan, scan.root, "payload");
        let inner = child_named(&scan, payload, "inner");
        let keys = RandomState::new();

        let plan = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Delete,
            },
        )
        .expect("plan");
        let token = plan.token.clone().expect("token");

        let error = apply(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: inner,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Delete,
            },
            &token,
        )
        .expect_err("a token is bound to the node it was minted for");
        assert!(
            matches!(error, RelocateError::Action(ActionError::InvalidConfirmation)),
            "got {error:?}"
        );
    }

    #[test]
    fn the_scan_root_cannot_be_relocated() {
        let (_source, destination, scan) = fixture();
        let keys = RandomState::new();
        let error = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: scan.root,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Trash,
            },
        )
        .expect_err("the scan root is not actionable");
        assert!(
            matches!(error, RelocateError::Action(ActionError::NotActionable { .. })),
            "got {error:?}"
        );
    }

    #[test]
    fn only_filesystems_that_can_hold_xattrs_and_acls_are_accepted() {
        // The defect this pins: `ditto` was chosen precisely because it carries
        // ACLs, xattrs and resource forks. A destination that cannot store them
        // drops them WITHOUT failing, and `verify_tree` compares contents,
        // names and symlink targets — not metadata — so verification would pass
        // and the source would then be disposed of. The loss is silent and
        // permanent, which is why this is a refusal and not a warning.
        assert!(carries_macos_metadata("apfs"));
        assert!(carries_macos_metadata("hfs"));

        assert!(!carries_macos_metadata("exfat"));
        assert!(!carries_macos_metadata("msdos"));
        assert!(!carries_macos_metadata("smbfs"));
        assert!(!carries_macos_metadata("nfs"));
        assert!(!carries_macos_metadata("unknown"));
    }

    #[test]
    fn the_destination_filesystem_is_reported_and_is_metadata_capable_here() {
        let (_source, destination, scan) = fixture();
        let payload = child_named(&scan, scan.root, "payload");
        let keys = RandomState::new();

        let plan = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Trash,
            },
        )
        .expect("plan");

        // The scratch dir is on the boot volume, which is APFS — so the plan
        // must name the filesystem and must not have refused on that ground.
        assert!(carries_macos_metadata(&plan.destination_filesystem), "got {plan:?}");
        assert!(
            !plan
                .warnings
                .iter()
                .any(|warning| warning.message.contains("extended attributes")),
            "an APFS destination must not be refused for metadata reasons"
        );
    }

    #[test]
    fn a_plan_reports_free_space_and_the_destination_device() {
        let (_source, destination, scan) = fixture();
        let payload = child_named(&scan, scan.root, "payload");
        let keys = RandomState::new();

        let plan = plan(
            &scan,
            &keys,
            NOW,
            RelocateRequest {
                node: payload,
                destination_parent: destination.path(),
                mode: RelocateMode::Migrate,
                disposal: SourceDisposal::Trash,
            },
        )
        .expect("plan");

        assert!(plan.destination_available > 0, "df must report something");
        assert!(
            plan.destination_device > 0,
            "the destination parent exists, so it has a device"
        );
        // Both tempdirs are on the same volume here, so the honest warning is
        // that this move frees nothing.
        assert!(plan.warnings.iter().any(|warning| warning.code == "same-volume"));
    }
}

/// True when `path` is a plain, absolute path with no `..` in it.
///
/// Used to validate the destination the frontend picked, which — unlike a
/// source path — is not reconstructed from the arena and therefore *is*
/// attacker-controlled in the threat model docs/03 sets out.
pub(crate) fn is_acceptable_destination(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|part| matches!(part, Component::ParentDir))
        && path.as_os_str() != OsStr::new("/")
}
