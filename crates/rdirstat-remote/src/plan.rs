//! Deciding what a remote destination is missing, before uploading anything.
//!
//! Same promise as the local sync in `src-tauri/src/sync.rs` — **nothing at the
//! destination is ever deleted, and nothing is overwritten unless the user asks
//! for it** — reached by a different algorithm, because the constraints are not
//! the same.
//!
//! ## Why this is not the local walk with a different `stat`
//!
//! The local planner walks the source and `stat`s the matching destination path
//! as it goes. Doing that remotely would be one network round trip per file: at
//! 40 ms of latency, a 100,000-file tree spends an hour deciding whether to
//! copy anything. So the remote planner inverts it — **one recursive listing up
//! front**, into a map, then a purely local walk that answers every membership
//! question from memory. One request per thousand objects instead of one per
//! object.
//!
//! ## The three things that have no local equivalent
//!
//! 1. **A remote key is UTF-8. A macOS filename is bytes.** A file named with
//!    an invalid UTF-8 sequence — rare, but real on volumes restored from old
//!    backups — has no representable S3 key. It is counted and skipped by name,
//!    never transliterated: silently mangling a filename during a backup is how
//!    a file becomes unfindable at restore time.
//! 2. **There is no `statfs` for a bucket.** The local planner refuses a copy
//!    that will not fit. Here the answer is genuinely unknown, so
//!    [`RemotePlan::destination_available`] is `None` — not zero, which would
//!    read as "full", and not `u64::MAX`, which would read as a promise.
//! 3. **"Same contents" costs money.** Locally, verifying means reading two
//!    files. Remotely it means downloading the object, so the only free
//!    evidence is a digest the endpoint already computed — and for S3 that
//!    digest is frequently unusable (see [`crate::backend::usable_etag`]).

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read as _};
use std::path::{Component, Path};

use md5::{Digest as _, Md5};
use rdirstat_core::DisplayPath;
use serde::{Deserialize, Serialize};

use crate::backend::{Comparison, RemoteEntry};

/// Ceiling on entries a plan will *list*. The counts stay exact past it.
///
/// Matches `sync::MAX_PLANNED_ENTRIES` deliberately: the two plans render in
/// the same table and a user should not find that one truncates at a different
/// point than the other.
pub const MAX_PLANNED_ENTRIES: usize = 5_000;

/// Chunk size for digesting a local file.
const DIGEST_CHUNK: usize = 256 * 1024;

/// Directories never descended into. macOS bookkeeping, not user data.
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    ".Spotlight-V100",
    ".fseventsd",
    ".TemporaryItems",
    ".Trashes",
    ".DocumentRevisions-V100",
];

/// What to do about a file that exists on both sides but differs.
///
/// Defined here rather than in `src-tauri` so the local and remote planners
/// share one type — and therefore one generated TypeScript type, and one
/// meaning of "Keep theirs" in the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum OnDiffer {
    /// Leave it. The destination keeps whatever it has. The default.
    Skip,
    /// Overwrite it from the source. The only setting that destroys anything.
    Replace,
}

/// How hard to look before calling a remote object the same as a local file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCompare {
    /// Path and size, from the listing alone. No extra requests, no local
    /// reads. The default, and the only mode that is free.
    Quick,
    /// Also compares digests where the endpoint published a usable one, which
    /// means reading every same-sized local file in full to hash it. Catches a
    /// file edited in place without changing length. Costs a full read of the
    /// overlap locally and nothing remotely.
    Verify,
}

/// Why one file is in the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RemoteReason {
    /// No object at that key.
    Missing,
    /// Present, but a different length.
    SizeDiffers,
    /// Present and the same length, but the digests disagree. Only ever
    /// produced under [`RemoteCompare::Verify`], and only where the endpoint
    /// published a digest that means what it appears to mean.
    ContentDiffers,
}

/// One file the transfer would upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RemoteSyncEntry {
    /// Path relative to the source root, which is also its key under the
    /// target's root. Shown to the user; never used as authority.
    pub relative_path: String,
    pub bytes: u64,
    pub reason: RemoteReason,
}

/// Something the user should read before confirming.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RemoteWarning {
    pub code: String,
    pub message: String,
}

impl RemoteWarning {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// What an upload would do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RemotePlan {
    pub source: DisplayPath,
    /// The target's address, credential-free.
    pub destination: String,
    pub compare: RemoteCompare,
    pub on_differ: OnDiffer,
    /// What evidence this endpoint could actually offer. Under
    /// [`RemoteCompare::Verify`] against a [`Comparison::Size`] endpoint, the
    /// plan degrades to size-only and says so in a warning rather than
    /// pretending it verified.
    pub available_comparison: Comparison,
    /// The files that would be uploaded, capped at [`MAX_PLANNED_ENTRIES`].
    pub entries: Vec<RemoteSyncEntry>,
    pub entries_truncated: bool,
    /// True count, even when `entries` was truncated.
    pub total_to_copy: u64,
    pub bytes_to_copy: u64,
    pub already_present: u64,
    /// Differ, and being left alone because `on_differ` is `Skip`.
    pub differing_skipped: u64,
    /// Sockets, FIFOs and device nodes. Nothing can upload them.
    pub special_skipped: u64,
    /// Files whose names are not valid UTF-8 and so have no remote key.
    pub unnameable_skipped: u64,
    /// Local directories that could not be read. The plan is a floor.
    pub unreadable: u64,
    /// **Always `None`.** A bucket has no free-space figure, and the field
    /// exists so that a UI written against the local plan renders "unknown"
    /// instead of a fabricated zero.
    pub destination_available: Option<u64>,
    /// The remote listing hit its cap, so some objects were not seen. Files
    /// past the cap will be treated as missing and re-uploaded.
    pub listing_truncated: bool,
    pub warnings: Vec<RemoteWarning>,
}

/// Running totals while walking the source.
#[derive(Debug, Default)]
struct Tally {
    total_to_copy: u64,
    bytes_to_copy: u64,
    already_present: u64,
    differing_skipped: u64,
    special_skipped: u64,
    unnameable_skipped: u64,
    unreadable: u64,
}

/// Describes what an upload to `destination` would do.
///
/// Takes the remote listing as an argument rather than fetching it, so the
/// whole decision procedure is a pure function of (local tree, remote listing,
/// settings) and can be tested exhaustively with no network.
#[must_use]
pub fn plan(
    source: &Path,
    destination: &str,
    listing: &[RemoteEntry],
    listing_truncated: bool,
    available_comparison: Comparison,
    compare: RemoteCompare,
    on_differ: OnDiffer,
) -> RemotePlan {
    let remote: HashMap<&str, &RemoteEntry> = listing
        .iter()
        .map(|entry| (entry.relative_path.as_str(), entry))
        .collect();

    // Asking to verify against an endpoint that publishes nothing comparable is
    // not an error — it is a request that cannot be honoured, and the plan says
    // which mode actually ran.
    let effective = match (compare, available_comparison) {
        (RemoteCompare::Verify, Comparison::SizeAndDigest) => RemoteCompare::Verify,
        _ => RemoteCompare::Quick,
    };

    let mut entries = Vec::new();
    let mut tally = Tally::default();
    walk(
        source,
        Path::new(""),
        &remote,
        effective,
        on_differ,
        &mut entries,
        &mut tally,
    );

    let warnings = advisories(
        &tally,
        compare,
        effective,
        available_comparison,
        listing_truncated,
        on_differ,
    );

    RemotePlan {
        source: DisplayPath::from_bytes(source.as_os_str().as_encoded_bytes()),
        destination: destination.to_owned(),
        compare: effective,
        on_differ,
        available_comparison,
        entries_truncated: tally.total_to_copy > entries.len() as u64,
        entries,
        total_to_copy: tally.total_to_copy,
        bytes_to_copy: tally.bytes_to_copy,
        already_present: tally.already_present,
        differing_skipped: tally.differing_skipped,
        special_skipped: tally.special_skipped,
        unnameable_skipped: tally.unnameable_skipped,
        unreadable: tally.unreadable,
        // Not a placeholder. See the type's doc comment.
        destination_available: None,
        listing_truncated,
        warnings,
    }
}

/// Walks one directory of the source, recursing.
fn walk(
    root: &Path,
    relative: &Path,
    remote: &HashMap<&str, &RemoteEntry>,
    compare: RemoteCompare,
    on_differ: OnDiffer,
    entries: &mut Vec<RemoteSyncEntry>,
    tally: &mut Tally,
) {
    let Ok(reader) = fs::read_dir(root.join(relative)) else {
        tally.unreadable = tally.unreadable.saturating_add(1);
        return;
    };

    for item in reader {
        let Ok(item) = item else {
            tally.unreadable = tally.unreadable.saturating_add(1);
            continue;
        };
        let name = item.file_name();
        let child = relative.join(&name);

        // `symlink_metadata`, not `metadata`: following a link would upload the
        // target's bytes under the link's name, and a link to a directory would
        // make the walk unbounded through a cycle.
        let Ok(metadata) = item.path().symlink_metadata() else {
            tally.unreadable = tally.unreadable.saturating_add(1);
            continue;
        };

        if metadata.is_dir() {
            if SKIPPED_DIRECTORY_NAMES.iter().any(|skipped| name == *skipped) {
                continue;
            }
            walk(root, &child, remote, compare, on_differ, entries, tally);
            continue;
        }

        if !metadata.is_file() {
            // A symlink, socket, FIFO or device node. Counted, not failed:
            // there is no remote representation, and aborting a 400 GB backup
            // because a build tree contains a socket helps nobody.
            tally.special_skipped = tally.special_skipped.saturating_add(1);
            continue;
        }

        let Some(key) = relative_key(&child) else {
            tally.unnameable_skipped = tally.unnameable_skipped.saturating_add(1);
            continue;
        };

        let bytes = metadata.len();
        let reason = match remote.get(key.as_str()) {
            None => Some(RemoteReason::Missing),
            Some(existing) if existing.bytes != bytes => Some(RemoteReason::SizeDiffers),
            Some(existing) => match (compare, &existing.digest) {
                (RemoteCompare::Verify, Some(digest)) => {
                    match local_digest(&root.join(&child)) {
                        Ok(local) if local.eq_ignore_ascii_case(digest) => None,
                        Ok(_) => Some(RemoteReason::ContentDiffers),
                        // Unreadable at plan time. Not "the same": counted as
                        // unreadable and left out, so the plan under-reports
                        // rather than claiming a file it never opened matches.
                        Err(_) => {
                            tally.unreadable = tally.unreadable.saturating_add(1);
                            continue;
                        }
                    }
                }
                _ => None,
            },
        };

        match reason {
            None => tally.already_present = tally.already_present.saturating_add(1),
            Some(RemoteReason::Missing) => {
                tally.total_to_copy = tally.total_to_copy.saturating_add(1);
                tally.bytes_to_copy = tally.bytes_to_copy.saturating_add(bytes);
                if entries.len() < MAX_PLANNED_ENTRIES {
                    entries.push(RemoteSyncEntry {
                        relative_path: key,
                        bytes,
                        reason: RemoteReason::Missing,
                    });
                }
            }
            Some(differing) if on_differ == OnDiffer::Replace => {
                tally.total_to_copy = tally.total_to_copy.saturating_add(1);
                tally.bytes_to_copy = tally.bytes_to_copy.saturating_add(bytes);
                if entries.len() < MAX_PLANNED_ENTRIES {
                    entries.push(RemoteSyncEntry {
                        relative_path: key,
                        bytes,
                        reason: differing,
                    });
                }
            }
            Some(_) => tally.differing_skipped = tally.differing_skipped.saturating_add(1),
        }
    }
}

/// A relative path as a remote key, or `None` when it cannot be one.
///
/// Returns `None` for a name that is not valid UTF-8 — see the module docs for
/// why that is a skip and not a lossy conversion — and for any component that
/// is not a plain name, which on this path would mean the walk produced
/// something it should not have.
fn relative_key(relative: &Path) -> Option<String> {
    let mut key = String::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        let part = part.to_str()?;
        if !key.is_empty() {
            key.push('/');
        }
        key.push_str(part);
    }
    (!key.is_empty()).then_some(key)
}

/// MD5 of a local file, to compare against the one S3 published.
///
/// MD5 and not something modern because the choice is not this app's: the only
/// digest an S3 endpoint hands back for free is `Content-MD5`/ETag. It is used
/// here to answer *did this file change*, never to authenticate anything, and a
/// collision an attacker would have to author on both sides of a backup they
/// already control is not a threat this defends against.
fn local_digest(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0_u8; DIGEST_CHUNK];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The plain-language notes shown above the plan.
fn advisories(
    tally: &Tally,
    requested: RemoteCompare,
    effective: RemoteCompare,
    available: Comparison,
    listing_truncated: bool,
    on_differ: OnDiffer,
) -> Vec<RemoteWarning> {
    let mut warnings = Vec::new();

    if requested == RemoteCompare::Verify && effective == RemoteCompare::Quick {
        warnings.push(RemoteWarning::new(
            "no_digest",
            format!(
                "This destination does not publish a checksum ({available:?}), so files were \
                 compared by size only. A file changed without changing length looks unchanged."
            ),
        ));
    }
    if listing_truncated {
        warnings.push(RemoteWarning::new(
            "listing_truncated",
            "The destination holds more objects than this app will list at once, so some were \
             not checked. Files it did not see are treated as missing and will be uploaded again.",
        ));
    }
    if tally.unnameable_skipped > 0 {
        warnings.push(RemoteWarning::new(
            "unnameable",
            format!(
                "{} file(s) have names that are not valid text and cannot be stored remotely. \
                 They were skipped rather than renamed.",
                tally.unnameable_skipped
            ),
        ));
    }
    if tally.unreadable > 0 {
        warnings.push(RemoteWarning::new(
            "unreadable",
            format!(
                "{} item(s) could not be read, so this list is a minimum. Granting Full Disk \
                 Access usually resolves it.",
                tally.unreadable
            ),
        ));
    }
    if tally.special_skipped > 0 {
        warnings.push(RemoteWarning::new(
            "special",
            format!(
                "{} item(s) are links, sockets or device nodes and have no remote equivalent.",
                tally.special_skipped
            ),
        ));
    }
    if on_differ == OnDiffer::Replace {
        warnings.push(RemoteWarning::new(
            "replacing",
            "Files that differ will be overwritten at the destination. This is the only setting \
             here that destroys anything.",
        ));
    }
    if tally.differing_skipped > 0 && on_differ == OnDiffer::Skip {
        warnings.push(RemoteWarning::new(
            "differing_kept",
            format!(
                "{} file(s) exist at the destination but differ. They are being left alone.",
                tally.differing_skipped
            ),
        ));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;

    fn entry(path: &str, bytes: u64, digest: Option<&str>) -> RemoteEntry {
        RemoteEntry {
            relative_path: path.to_owned(),
            bytes,
            digest: digest.map(str::to_owned),
        }
    }

    fn write(root: &Path, relative: &str, contents: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the fixture directory should be creatable");
        }
        File::create(&path)
            .expect("the fixture file should be creatable")
            .write_all(contents)
            .expect("the fixture file should be writable");
    }

    fn quick(source: &Path, listing: &[RemoteEntry]) -> RemotePlan {
        plan(
            source,
            "s3://b/",
            listing,
            false,
            Comparison::Size,
            RemoteCompare::Quick,
            OnDiffer::Skip,
        )
    }

    #[test]
    fn an_empty_destination_takes_everything() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");
        write(dir.path(), "nested/b.txt", b"world!");

        let result = quick(dir.path(), &[]);
        assert_eq!(result.total_to_copy, 2);
        assert_eq!(result.bytes_to_copy, 11);
        assert_eq!(result.already_present, 0);
        // The one field a UI must not read as "the bucket is full".
        assert_eq!(result.destination_available, None);
    }

    #[test]
    fn a_same_sized_object_is_present_under_quick() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        let result = quick(dir.path(), &[entry("a.txt", 5, None)]);
        assert_eq!(result.total_to_copy, 0);
        assert_eq!(result.already_present, 1);
    }

    #[test]
    fn a_differently_sized_object_is_kept_by_default_and_counted() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        let result = quick(dir.path(), &[entry("a.txt", 99, None)]);
        assert_eq!(result.total_to_copy, 0);
        assert_eq!(result.differing_skipped, 1);
        assert!(result.warnings.iter().any(|warning| warning.code == "differing_kept"));
    }

    #[test]
    fn replace_uploads_what_skip_would_keep() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        let result = plan(
            dir.path(),
            "s3://b/",
            &[entry("a.txt", 99, None)],
            false,
            Comparison::Size,
            RemoteCompare::Quick,
            OnDiffer::Replace,
        );
        assert_eq!(result.total_to_copy, 1);
        assert_eq!(result.entries[0].reason, RemoteReason::SizeDiffers);
        assert!(result.warnings.iter().any(|warning| warning.code == "replacing"));
    }

    // The case size alone cannot see: same length, different bytes.
    #[test]
    fn verify_catches_an_in_place_edit_that_kept_the_length() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        // MD5("hello") is 5d41402a…; this is a different digest of the same length.
        let listing = [entry("a.txt", 5, Some("00000000000000000000000000000000"))];
        let result = plan(
            dir.path(),
            "s3://b/",
            &listing,
            false,
            Comparison::SizeAndDigest,
            RemoteCompare::Verify,
            OnDiffer::Replace,
        );
        assert_eq!(result.total_to_copy, 1);
        assert_eq!(result.entries[0].reason, RemoteReason::ContentDiffers);
    }

    #[test]
    fn verify_accepts_a_matching_digest() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        let digest = local_digest(&dir.path().join("a.txt")).expect("a file just written is readable");
        assert_eq!(digest, "5d41402abc4b2a76b9719d911017c592");

        let listing = [entry("a.txt", 5, Some(&digest))];
        let result = plan(
            dir.path(),
            "s3://b/",
            &listing,
            false,
            Comparison::SizeAndDigest,
            RemoteCompare::Verify,
            OnDiffer::Replace,
        );
        assert_eq!(result.total_to_copy, 0);
        assert_eq!(result.already_present, 1);
    }

    // Asking to verify against an endpoint with no digest must not silently
    // report that it verified.
    #[test]
    fn verify_degrades_to_size_and_admits_it() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        let result = plan(
            dir.path(),
            "s3://b/",
            &[entry("a.txt", 5, None)],
            false,
            Comparison::Size,
            RemoteCompare::Verify,
            OnDiffer::Skip,
        );
        assert_eq!(result.compare, RemoteCompare::Quick);
        assert!(result.warnings.iter().any(|warning| warning.code == "no_digest"));
    }

    #[test]
    fn a_truncated_listing_is_declared_not_swallowed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "a.txt", b"hello");

        let result = plan(
            dir.path(),
            "s3://b/",
            &[],
            true,
            Comparison::Size,
            RemoteCompare::Quick,
            OnDiffer::Skip,
        );
        assert!(result.listing_truncated);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.code == "listing_truncated")
        );
    }

    #[test]
    fn macos_bookkeeping_directories_are_not_uploaded() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), ".Spotlight-V100/index", b"junk");
        write(dir.path(), "a.txt", b"hello");

        let result = quick(dir.path(), &[]);
        assert_eq!(result.total_to_copy, 1);
        assert_eq!(result.entries[0].relative_path, "a.txt");
    }

    #[test]
    fn a_relative_key_uses_forward_slashes_and_no_leading_one() {
        assert_eq!(relative_key(Path::new("a/b/c.txt")).as_deref(), Some("a/b/c.txt"));
        assert_eq!(relative_key(Path::new("")), None);
    }

    // A filename is bytes; an S3 key is UTF-8. The mismatch is real and must be
    // a visible skip, never a transliteration.
    //
    // Tested against the pure function rather than through a fixture, because
    // **APFS will not create such a file**: `File::create` on a name that is
    // not valid UTF-8 fails with EILSEQ, "Illegal byte sequence". Which is
    // exactly why this is not dead code — the names that reach it come from the
    // volumes this app is pointed at rather than the one it runs on: exFAT and
    // FAT32 sticks, NTFS disks, and SMB shares all store arbitrary bytes, and a
    // file copied from one of those onto a scanned mount arrives intact.
    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_has_no_remote_key() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let bad = Path::new(OsStr::from_bytes(b"broken-\xff\xfe.bin"));
        assert_eq!(relative_key(bad), None);

        let nested = Path::new("folder").join(OsStr::from_bytes(b"\xc3\x28.bin"));
        assert_eq!(
            relative_key(&nested),
            None,
            "one bad segment disqualifies the whole key"
        );

        // The neighbouring good name still works, so the skip is per file.
        assert_eq!(
            relative_key(Path::new("folder/fine.bin")).as_deref(),
            Some("folder/fine.bin")
        );
    }

    // The tally and the warning that go with that skip, driven directly since
    // the filesystem will not produce the input.
    #[test]
    fn an_unnameable_file_produces_a_warning_that_says_it_was_not_renamed() {
        let tally = Tally {
            unnameable_skipped: 3,
            ..Tally::default()
        };
        let warnings = advisories(
            &tally,
            RemoteCompare::Quick,
            RemoteCompare::Quick,
            Comparison::Size,
            false,
            OnDiffer::Skip,
        );
        let warning = warnings
            .iter()
            .find(|warning| warning.code == "unnameable")
            .expect("a warning");
        assert!(warning.message.contains('3'), "{}", warning.message);
        assert!(warning.message.contains("skipped"), "{}", warning.message);
    }

    // Following a symlink would upload the target's bytes under the link's
    // name, and a link to a parent would make the walk unbounded.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_counted_not_followed() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        write(dir.path(), "real.txt", b"hello");
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .expect("a symlink inside a temporary directory");

        let result = quick(dir.path(), &[]);
        assert_eq!(result.total_to_copy, 1);
        assert_eq!(result.special_skipped, 1);
    }

    #[test]
    fn the_listing_is_capped_but_the_counts_are_not() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        for index in 0..(MAX_PLANNED_ENTRIES + 10) {
            write(dir.path(), &format!("f{index}.bin"), b"x");
        }

        let result = quick(dir.path(), &[]);
        assert_eq!(result.entries.len(), MAX_PLANNED_ENTRIES);
        assert!(result.entries_truncated);
        assert_eq!(result.total_to_copy, MAX_PLANNED_ENTRIES as u64 + 10);
    }
}
