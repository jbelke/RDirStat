//! The 48-byte arena node, its kind, and its flag bits.

use core::mem::{align_of, size_of};

use serde::{Deserialize, Serialize};

use crate::id::{CategoryId, NodeId};
use crate::name::NameRef;

/// What kind of filesystem object a node is.
///
/// Stored as [`Node::kind`], a `u8`. `#[non_exhaustive]` because filesystem
/// facts grow: a variant added here must not break a `match` in a sibling
/// crate.
///
/// Special kinds (socket, device, FIFO) are leaves with zero data contribution
/// unless platform metadata says otherwise, and their contents are never opened
/// (docs/02-SCANNER.md#traversal-rules).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
#[non_exhaustive]
pub enum Kind {
    /// Anything the reader could not classify, including a metadata failure.
    #[default]
    Unknown = 0,
    /// A regular file.
    File = 1,
    /// A directory. Only directories appear in the [`DirIndex`](crate::DirIndex).
    Directory = 2,
    /// A symbolic link. Always a leaf: the scanner never follows one.
    Symlink = 3,
    /// A Unix domain socket.
    Socket = 4,
    /// A named pipe.
    Fifo = 5,
    /// A character device.
    CharDevice = 6,
    /// A block device.
    BlockDevice = 7,
}

impl Kind {
    /// Decodes the `u8` stored in [`Node::kind`].
    ///
    /// An unrecognized byte becomes [`Kind::Unknown`] rather than an error: a
    /// snapshot written by a newer build must load with reduced fidelity, not
    /// fail.
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::File,
            2 => Self::Directory,
            3 => Self::Symlink,
            4 => Self::Socket,
            5 => Self::Fifo,
            6 => Self::CharDevice,
            7 => Self::BlockDevice,
            _ => Self::Unknown,
        }
    }

    /// The `u8` to store in [`Node::kind`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this kind may have children and a [`DirTotals`](crate::DirTotals)
    /// row.
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Whether this kind contributes file bytes to rollups.
    #[must_use]
    pub const fn is_file(self) -> bool {
        matches!(self, Self::File)
    }

    /// Whether this kind is a leaf by policy, regardless of what it points at.
    #[must_use]
    pub const fn is_leaf(self) -> bool {
        !self.is_directory()
    }

    /// A stable, untranslated key for persistence and report grouping.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Socket => "socket",
            Self::Fifo => "fifo",
            Self::CharDevice => "char_device",
            Self::BlockDevice => "block_device",
        }
    }
}

/// Bit constants for [`Node::flags`].
///
/// Sixteen bits, thirteen assigned. Bits 13–15 are reserved and must be written
/// as zero; a reader that finds them set treats the node as valid and ignores
/// them, which is what lets an older build open a newer snapshot.
pub mod flags {
    /// No flags.
    pub const NONE: u16 = 0;

    /// The directory could not be read (`EACCES`/`EPERM`). Its subtree totals
    /// are a floor, not a total.
    pub const UNREADABLE: u16 = 1 << 0;

    /// The directory sits on a different device than the scan root. Retained as
    /// a marker; not descended unless `cross_filesystems` is enabled.
    pub const MOUNT_POINT: u16 = 1 << 1;

    /// Darwin `SF_FIRMLINK`. Never descended during a root-volume scan, because
    /// doing so walks the data volume twice.
    pub const FIRMLINK: u16 = 1 << 2;

    /// An exclusion rule matched this path, so it was not opened. Reported
    /// separately from unreadable paths.
    pub const EXCLUDED: u16 = 1 << 3;

    /// `nlink > 1`: the content is reachable by more than one name.
    pub const HARD_LINK: u16 = 1 << 4;

    /// A later observation of a `(dev, ino)` already counted in this scan. The
    /// entry stays visible but contributes **zero** bytes to rollups. The UI
    /// says "counted at <path>" rather than implying this path owns the bytes.
    pub const HARD_LINK_REPEAT: u16 = 1 << 5;

    /// Allocated bytes are below logical bytes — a sparse or compressed file.
    /// A useful signal, not a reclaimable-bytes claim.
    pub const SPARSE: u16 = 1 << 6;

    /// A regular file with an execute bit. Feeds the Executable category
    /// fallback when no suffix matches.
    pub const EXECUTABLE: u16 = 1 << 7;

    /// A directory users perceive as a document: `.app`, `.photoslibrary`,
    /// `.xcodeproj`, and friends. The scanner still descends and accounts for
    /// the contents; presentation collapses it by default.
    pub const PACKAGE: u16 = 1 << 8;

    /// Children below `--aggregate-below` were folded into this node's totals
    /// instead of being retained. Their paths cannot be searched, trashed,
    /// catalogued, or used in exact per-suffix statistics.
    pub const AGGREGATED: u16 = 1 << 9;

    /// The entry changed or vanished between enumeration and metadata. Counted
    /// as a mutation; the recorded values are best-effort.
    pub const MUTATED: u16 = 1 << 10;

    /// The directory listing did not complete, so its child set is partial for
    /// a reason other than a permission error.
    pub const INCOMPLETE: u16 = 1 << 11;

    /// A symlink whose target did not resolve at scan time.
    pub const BROKEN_SYMLINK: u16 = 1 << 12;

    /// Every assigned bit. `flags & !ASSIGNED` is the reserved space.
    pub const ASSIGNED: u16 = UNREADABLE
        | MOUNT_POINT
        | FIRMLINK
        | EXCLUDED
        | HARD_LINK
        | HARD_LINK_REPEAT
        | SPARSE
        | EXECUTABLE
        | PACKAGE
        | AGGREGATED
        | MUTATED
        | INCOMPLETE
        | BROKEN_SYMLINK;

    /// Flags that mean "this node's subtree total is incomplete". The UI must
    /// label a total that includes one of these.
    pub const INCOMPLETE_SUBTREE: u16 = UNREADABLE | EXCLUDED | INCOMPLETE | AGGREGATED;
}

/// One retained filesystem entry.
///
/// Exactly 48 bytes with `#[repr(C)]` and no padding, asserted below. At the
/// 69M-entry design profile the node array alone is 3.08 GiB, so a single
/// added `u64` costs half a gigabyte and must go through the memory table in
/// docs/01-ARCHITECTURE.md#memory-budget first.
///
/// Fields are public because the builder in `rdirstat-scan` writes them
/// directly on the hot path; the accessors below exist so readers never
/// re-derive the encoding.
///
/// Deliberately **not** stored here: the full path (walk parents instead),
/// `dev`/`ino` (scan-time identity, kept in the hard-link set and the catalog
/// row), `nlink`, and `mode` (folded into [`flags::EXECUTABLE`] and the
/// category).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Node {
    /// Logical bytes for this entry (`st_size`). Zero for directories: a
    /// directory's own inode size is not user data and would inflate every
    /// total.
    pub size: u64,
    /// Filesystem-reported allocated bytes (`st_blocks * 512`). On APFS this
    /// is not a promise of uniquely reclaimable space.
    pub alloc: u64,
    /// Modification time in whole Unix seconds. Signed, so pre-1970 timestamps
    /// survive instead of clamping.
    pub mtime: i64,
    /// This entry's own name component, as lossless bytes in the tree's
    /// [`NameBlob`](crate::NameBlob).
    pub name: NameRef,
    /// Parent node, or [`NodeId::NONE`] for the root.
    pub parent: NodeId,
    /// First child, or [`NodeId::NONE`]. Insertion order is not a UI contract;
    /// queries sort.
    pub first_child: NodeId,
    /// Next sibling under the same parent, or [`NodeId::NONE`].
    pub next_sibling: NodeId,
    /// Bit set from the [`flags`] module.
    pub flags: u16,
    /// Primary content category, as [`CategoryId`].
    pub category: u8,
    /// Filesystem object kind, as [`Kind`].
    pub kind: u8,
}

// The budget from docs/08-RUST-PRACTICES.md, asserted so it cannot regress
// silently. The `== 48` assertion is the stronger claim: it also catches a
// reordering that introduces padding.
const _: () = assert!(size_of::<Node>() <= 48);
const _: () = assert!(size_of::<Node>() == 48);
const _: () = assert!(align_of::<Node>() == 8);
const _: () = assert!(size_of::<NodeId>() == 4);
const _: () = assert!(size_of::<NameRef>() == 8);

impl Node {
    /// A detached node with no links, no name, and no bytes.
    pub const EMPTY: Self = Self {
        size: 0,
        alloc: 0,
        mtime: 0,
        name: NameRef::EMPTY,
        parent: NodeId::NONE,
        first_child: NodeId::NONE,
        next_sibling: NodeId::NONE,
        flags: flags::NONE,
        category: 0,
        kind: Kind::Unknown as u8,
    };

    /// A leaf with the given metadata and no links yet.
    #[must_use]
    pub const fn leaf(name: NameRef, kind: Kind, size: u64, alloc: u64, mtime: i64) -> Self {
        Self {
            size,
            alloc,
            mtime,
            name,
            ..Self::EMPTY
        }
        .with_kind(kind)
    }

    /// A directory with no children yet. Directories carry zero own bytes;
    /// their subtree totals live in [`DirTotals`](crate::DirTotals).
    #[must_use]
    pub const fn directory(name: NameRef, mtime: i64) -> Self {
        Self {
            mtime,
            name,
            ..Self::EMPTY
        }
        .with_kind(Kind::Directory)
    }

    /// Replaces the kind.
    #[must_use]
    pub const fn with_kind(mut self, kind: Kind) -> Self {
        self.kind = kind.as_u8();
        self
    }

    /// Replaces the category.
    #[must_use]
    pub const fn with_category(mut self, category: CategoryId) -> Self {
        self.category = category.get();
        self
    }

    /// Adds flag bits.
    #[must_use]
    pub const fn with_flags(mut self, bits: u16) -> Self {
        self.flags |= bits;
        self
    }

    /// The decoded kind.
    #[must_use]
    pub const fn kind(self) -> Kind {
        Kind::from_u8(self.kind)
    }

    /// The decoded category.
    #[must_use]
    pub const fn category(self) -> CategoryId {
        CategoryId::from_raw(self.category)
    }

    /// Whether every bit in `bits` is set.
    #[must_use]
    pub const fn has_flags(self, bits: u16) -> bool {
        self.flags & bits == bits
    }

    /// Whether any bit in `bits` is set.
    #[must_use]
    pub const fn has_any_flag(self, bits: u16) -> bool {
        self.flags & bits != 0
    }

    /// Logical bytes this node contributes to a rollup.
    ///
    /// Zero for a repeated hard link, because the content was already counted
    /// under the path that was traversed first. This is the single place that
    /// policy is applied, so a sibling crate cannot implement a second version
    /// of it.
    #[must_use]
    pub const fn contributed_size(self) -> u64 {
        if self.has_flags(flags::HARD_LINK_REPEAT) {
            0
        } else {
            self.size
        }
    }

    /// Allocated bytes this node contributes to a rollup. Same policy as
    /// [`Node::contributed_size`].
    #[must_use]
    pub const fn contributed_alloc(self) -> u64 {
        if self.has_flags(flags::HARD_LINK_REPEAT) {
            0
        } else {
            self.alloc
        }
    }

    /// Whether this node has at least one child link.
    #[must_use]
    pub const fn has_children(self) -> bool {
        !self.first_child.is_none()
    }
}

impl Default for Node {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_exactly_the_budget() {
        assert_eq!(size_of::<Node>(), 48);
        assert_eq!(align_of::<Node>(), 8);
    }

    #[test]
    fn unknown_kind_bytes_degrade_rather_than_fail() {
        assert_eq!(Kind::from_u8(2), Kind::Directory);
        assert_eq!(Kind::from_u8(200), Kind::Unknown);
        for kind in [
            Kind::Unknown,
            Kind::File,
            Kind::Directory,
            Kind::Symlink,
            Kind::Socket,
            Kind::Fifo,
            Kind::CharDevice,
            Kind::BlockDevice,
        ] {
            assert_eq!(Kind::from_u8(kind.as_u8()), kind);
        }
    }

    #[test]
    fn repeated_hard_links_stay_visible_but_contribute_nothing() {
        let node = Node::leaf(NameRef::EMPTY, Kind::File, 4096, 4096, 0)
            .with_flags(flags::HARD_LINK | flags::HARD_LINK_REPEAT);
        assert_eq!(node.size, 4096, "the entry keeps its real size for display");
        assert_eq!(node.contributed_size(), 0);
        assert_eq!(node.contributed_alloc(), 0);

        let first = Node::leaf(NameRef::EMPTY, Kind::File, 4096, 4096, 0).with_flags(flags::HARD_LINK);
        assert_eq!(first.contributed_size(), 4096);
    }

    #[test]
    fn reserved_flag_bits_are_disjoint_from_assigned_ones() {
        assert_eq!(flags::ASSIGNED & !0x1fff, 0);
        assert_eq!(flags::ASSIGNED.count_ones(), 13);
    }

    #[test]
    fn empty_node_has_no_links() {
        assert!(Node::EMPTY.parent.is_none());
        assert!(!Node::EMPTY.has_children());
        assert_eq!(Node::EMPTY.kind(), Kind::Unknown);
        assert_eq!(Node::EMPTY.category(), CategoryId::UNCATEGORIZED);
    }
}
