//! What a reader hands the builder: lossless name bytes plus the metadata the
//! arena and the traversal policy need.
//!
//! Nothing here allocates per entry on the common path: [`SmallName`] keeps
//! short names inline, and a [`RawEntryBatch`] borrows the reader's reused
//! buffer rather than owning one.

use core::fmt;

use rdirstat_core::{ErrorClass, Kind, Operation};
use smallvec::SmallVec;

/// Inline capacity for [`SmallName`], in bytes.
///
/// **Not yet measured.** docs/08-RUST-PRACTICES.md asks for a value chosen from
/// a real name-length profile; 32 is a placeholder that covers the great
/// majority of macOS names and keeps [`RawEntry`] at a cache-friendly size. It
/// is a single constant so a measurement can move it without touching anything
/// else.
pub const INLINE_NAME_BYTES: usize = 32;

/// One entry name, as the lossless bytes the filesystem produced.
///
/// macOS names are NFD byte sequences and are not guaranteed to be UTF-8, so
/// nothing here converts to `String`. Display escaping happens once, at the
/// boundary, through [`DisplayPath`](rdirstat_core::DisplayPath).
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SmallName(SmallVec<[u8; INLINE_NAME_BYTES]>);

impl SmallName {
    /// An empty name.
    #[must_use]
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    /// Copies `bytes` into inline storage, spilling to the heap only if the
    /// name is longer than [`INLINE_NAME_BYTES`].
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(SmallVec::from_slice(bytes))
    }

    /// The name bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the name is empty, which a well-behaved filesystem never
    /// produces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether the name is `.` or `..`.
    ///
    /// `read_dir` already omits both; a bulk reader does not, so the check
    /// lives here where every reader can share it.
    #[must_use]
    pub fn is_dot_entry(&self) -> bool {
        matches!(self.as_bytes(), b"." | b"..")
    }
}

impl fmt::Debug for SmallName {
    /// Escaped, because a name is arbitrary bytes and a log line is not a place
    /// to discover that.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self.as_bytes()))
    }
}

/// One directory entry with the metadata the scanner needs.
///
/// Field-for-field the shape fixed in docs/02-SCANNER.md#reader-contract. Every
/// reader must produce these values with equivalent semantics; the differential
/// test is what keeps a faster reader from disagreeing silently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntry {
    /// Lossless name bytes; inline storage is an optimization, not a contract.
    pub name: SmallName,
    /// Object kind, observed without following a final symlink.
    pub kind: Kind,
    /// Logical bytes (`st_size`).
    pub size: u64,
    /// Allocated bytes (`st_blocks * 512`).
    pub alloc: u64,
    /// Modification time in whole Unix seconds.
    pub mtime: i64,
    /// Device the entry lives on, for the volume-boundary rule.
    pub dev: u64,
    /// Inode number, for the hard-link rule.
    pub ino: u64,
    /// Link count. `> 1` on a regular file means hard-linked content.
    pub nlink: u64,
    /// Permission bits, used only to classify executables. Not retained on
    /// [`Node`](rdirstat_core::Node).
    pub mode: u32,
    /// Platform flag bits; on Darwin this carries `st_flags`, including
    /// [`SF_FIRMLINK`](crate::SF_FIRMLINK).
    pub platform_flags: u32,
}

impl RawEntry {
    /// An entry with a name and kind and nothing else, for tests and for
    /// readers that could not stat an entry.
    #[must_use]
    pub fn stub(name: &[u8], kind: Kind) -> Self {
        Self {
            name: SmallName::from_bytes(name),
            kind,
            size: 0,
            alloc: 0,
            mtime: 0,
            dev: 0,
            ino: 0,
            nlink: 1,
            mode: 0,
            platform_flags: 0,
        }
    }
}

/// One entry that could not be read, inside an otherwise readable directory.
///
/// Per-entry failures are normal input, not an excuse to abandon the
/// directory: an entry that vanished between enumeration and `stat` is a race,
/// and the scan records it and keeps going.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryError {
    /// The entry's name, when enumeration got that far.
    pub name: SmallName,
    /// What was being attempted.
    pub operation: Operation,
    /// Stable class.
    pub class: ErrorClass,
    /// Raw `errno`, or `0` when the failure was not an OS error.
    pub os_code: i32,
}

/// A bounded run of entries from one directory.
///
/// Borrowed, not owned: the reader reuses its buffer across directories and the
/// builder folds the batch in before the reader refills it. Both the entry
/// count and the total name bytes are bounded, so a directory with ten million
/// children does not become one ten-million-element allocation.
#[derive(Clone, Copy, Debug)]
pub struct RawEntryBatch<'a> {
    entries: &'a [RawEntry],
    errors: &'a [EntryError],
}

impl<'a> RawEntryBatch<'a> {
    /// Wraps a slice of entries and the per-entry failures observed alongside
    /// them.
    #[must_use]
    pub const fn new(entries: &'a [RawEntry], errors: &'a [EntryError]) -> Self {
        Self { entries, errors }
    }

    /// The entries.
    #[must_use]
    pub const fn entries(&self) -> &'a [RawEntry] {
        self.entries
    }

    /// The per-entry failures.
    #[must_use]
    pub const fn errors(&self) -> &'a [EntryError] {
        self.errors
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the batch carries neither entries nor errors.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.errors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_name_stays_inline() {
        let name = SmallName::from_bytes(b"Applications");
        assert_eq!(name.as_bytes(), b"Applications");
        assert_eq!(name.len(), 12);
        assert!(!name.is_empty());
        assert!(!name.is_dot_entry());
    }

    #[test]
    fn names_are_bytes_not_utf8() {
        let name = SmallName::from_bytes(&[0xff, 0xfe, b'a']);
        assert_eq!(name.as_bytes(), &[0xff, 0xfe, b'a']);
        assert!(format!("{name:?}").contains('a'), "debug escapes rather than panicking");
    }

    #[test]
    fn dot_entries_are_recognized_for_readers_that_return_them() {
        assert!(SmallName::from_bytes(b".").is_dot_entry());
        assert!(SmallName::from_bytes(b"..").is_dot_entry());
        assert!(!SmallName::from_bytes(b"...").is_dot_entry());
    }

    #[test]
    fn a_long_name_spills_without_losing_bytes() {
        let long = vec![b'x'; INLINE_NAME_BYTES * 3];
        let name = SmallName::from_bytes(&long);
        assert_eq!(name.len(), INLINE_NAME_BYTES * 3);
        assert_eq!(name.as_bytes(), long.as_slice());
    }
}
