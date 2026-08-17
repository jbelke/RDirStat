//! Packed identifiers. Everything in the arena is an integer index, so every one
//! of them is a `#[repr(transparent)]` newtype: `fn children(&self, id: u32,
//! sort: u32)` is an API whose two arguments are silently swappable, and this
//! crate refuses to offer that API.
//!
//! None of these types derive arithmetic. `Bytes + Bytes` is meaningful;
//! `NodeId + NodeId` never is.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::fmt_bytes::{format_iec, format_si};

/// Bit 31 of a [`NodeId`] tags a virtual "files directly in this directory"
/// group whose remaining bits name its owning directory.
pub const VIRTUAL_GROUP_BIT: u32 = 1 << 31;

/// Highest arena slot a stored node may occupy.
///
/// One below [`NodeId::NONE`], which itself sits one below the tag bit, so the
/// arena holds at most `2^31 - 1` nodes.
pub const MAX_NODE_INDEX: u32 = (1 << 31) - 2;

/// Index of a node in the arena, or the sentinel [`NodeId::NONE`], or a tagged
/// virtual direct-files group.
///
/// The encoding is total and has exactly three disjoint cases:
///
/// | Raw value                        | Meaning                                  |
/// | -------------------------------- | ---------------------------------------- |
/// | `0 ..= MAX_NODE_INDEX`           | arena slot                               |
/// | `NodeId::NONE` (`0x7FFF_FFFF`)   | absent link — no parent, no child        |
/// | `VIRTUAL_GROUP_BIT \| dir_index` | the `<Files>` group owned by `dir_index` |
///
/// `NONE` is a sentinel rather than `Option<NodeId>` because an `Option` would
/// double the field width and break the 48-byte [`Node`](crate::Node) budget.
///
/// `0xFFFF_FFFF` is reserved: it would tag the group of the sentinel, which is
/// not a directory. [`NodeId::is_valid`] rejects it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct NodeId(u32);

impl NodeId {
    /// The absent-link sentinel. Not an arena slot and not a virtual group.
    pub const NONE: Self = Self((1 << 31) - 1);

    /// The first arena slot. A tree's root is conventionally, but not
    /// necessarily, this value.
    pub const ROOT: Self = Self(0);

    /// Builds an id for arena slot `index`.
    ///
    /// Returns `None` when `index > MAX_NODE_INDEX`, which is the ceiling that
    /// keeps bit 31 free for virtual groups.
    #[must_use]
    pub const fn from_index(index: u32) -> Option<Self> {
        if index <= MAX_NODE_INDEX {
            Some(Self(index))
        } else {
            None
        }
    }

    /// Reinterprets a raw wire value without validating it.
    ///
    /// The value is checked at the point of use — [`NodeId::index`] and every
    /// [`Tree`](crate::Tree) accessor return `None` for an id that does not
    /// name a live node. A `u32` arriving from IPC is never trusted as an
    /// index.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw bit pattern, for serialization and snapshot writing.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The arena slot this id names, or `None` for [`NodeId::NONE`], a virtual
    /// group, or the reserved value.
    #[must_use]
    pub const fn index(self) -> Option<u32> {
        if self.0 <= MAX_NODE_INDEX { Some(self.0) } else { None }
    }

    /// The arena slot as a `usize`, for indexing a node vector.
    #[must_use]
    pub const fn slot(self) -> Option<usize> {
        match self.index() {
            Some(index) => Some(index as usize),
            None => None,
        }
    }

    /// Whether this is the absent-link sentinel.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == Self::NONE.0
    }

    /// Whether this id names a real arena slot.
    #[must_use]
    pub const fn is_node(self) -> bool {
        self.0 <= MAX_NODE_INDEX
    }

    /// Whether bit 31 is set, i.e. this is a virtual direct-files group.
    ///
    /// A virtual group is a view derived from its directory's
    /// [`DirTotals`](crate::DirTotals). It is never an arena node, is never
    /// revealed or trashed as a path, and has no children.
    #[must_use]
    pub const fn is_virtual_group(self) -> bool {
        self.0 & VIRTUAL_GROUP_BIT != 0 && self.0 != u32::MAX
    }

    /// Whether this id is either a real node or a well-formed virtual group.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.is_node() || self.is_virtual_group()
    }

    /// Tags `directory` as its own virtual `<Files>` group.
    ///
    /// Returns `None` unless `directory` names a real arena slot; the sentinel
    /// and an already-tagged id have no group.
    #[must_use]
    pub const fn virtual_group_of(directory: Self) -> Option<Self> {
        match directory.index() {
            Some(index) => Some(Self(index | VIRTUAL_GROUP_BIT)),
            None => None,
        }
    }

    /// The directory that owns this virtual group, or `None` if this id is not
    /// a virtual group.
    #[must_use]
    pub const fn group_owner(self) -> Option<Self> {
        if self.is_virtual_group() {
            Some(Self(self.0 & !VIRTUAL_GROUP_BIT))
        } else {
            None
        }
    }
}

impl Default for NodeId {
    /// [`NodeId::NONE`] — an unset link, never slot zero.
    fn default() -> Self {
        Self::NONE
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            f.write_str("NodeId::NONE")
        } else if let Some(owner) = self.group_owner() {
            write!(f, "NodeId::group({})", owner.0)
        } else if let Some(index) = self.index() {
            write!(f, "NodeId({index})")
        } else {
            write!(f, "NodeId::RESERVED({:#010x})", self.0)
        }
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Position of a directory inside the parallel [`DirIndex`](crate::DirIndex)
/// vectors.
///
/// Distinct from [`NodeId`]: a `DirId` indexes the directory side tables, a
/// `NodeId` indexes the arena. Resolving one to the other is a binary search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct DirId(u32);

impl DirId {
    /// Wraps a directory-table position.
    #[must_use]
    pub const fn from_index(index: u32) -> Self {
        Self(index)
    }

    /// The directory-table position.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }

    /// The directory-table position as a `usize`.
    #[must_use]
    pub const fn slot(self) -> usize {
        self.0 as usize
    }
}

/// Monotonically increasing identifier for one scan attempt.
///
/// Every scan gets one, including scans that are cancelled or fail. The
/// frontend ignores progress events whose `ScanId` is not the one it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct ScanId(u64);

impl ScanId {
    /// The first id handed out in a process.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw counter value.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next id in sequence. Saturates rather than wrapping, so ordering can
    /// never invert.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl Default for ScanId {
    /// Zero — "no scan has run". [`ScanId::FIRST`] is the first id handed out.
    fn default() -> Self {
        Self(0)
    }
}

impl fmt::Display for ScanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scan#{}", self.0)
    }
}

/// Monotonically increasing identifier for one *published* tree.
///
/// Commands take a generation and reject a stale one rather than applying an
/// old selection, cursor, or confirmation token to a new tree. A cancelled or
/// failed scan never produces a generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct TreeGeneration(u64);

impl TreeGeneration {
    /// The generation of "no tree has been published yet".
    pub const NONE: Self = Self(0);

    /// The first published tree.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw counter value.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next generation in sequence. Saturates rather than wrapping.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether any tree has been published.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl Default for TreeGeneration {
    /// [`TreeGeneration::NONE`] — nothing has been published.
    fn default() -> Self {
        Self::NONE
    }
}

impl fmt::Display for TreeGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen#{}", self.0)
    }
}

/// Identifier of one completed scan partition in the optional catalog.
///
/// Distinct from [`ScanId`]: a `ScanId` is process-local and covers attempts, a
/// `CatalogScanId` names a durable, reconciled Parquet partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct CatalogScanId(u64);

impl CatalogScanId {
    /// Wraps a raw manifest key.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw manifest key.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Primary content category of a node, as assigned by `rdirstat-classify`.
///
/// Defined here rather than in `rdirstat-classify` because
/// [`Node`](crate::Node) stores it and `classify` depends on `core`, not the
/// other way round. `rdirstat-classify` re-exports this type and must not
/// define a second one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct CategoryId(u8);

impl CategoryId {
    /// Category 0 is always Uncategorized. A category table may not reuse it.
    pub const UNCATEGORIZED: Self = Self(0);

    /// The number of categories a configuration may define, including
    /// [`CategoryId::UNCATEGORIZED`].
    pub const MAX_CATEGORIES: usize = 256;

    /// Wraps a raw category index.
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        Self(raw)
    }

    /// The raw category index, as stored in [`Node::category`](crate::Node).
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for CategoryId {
    fn default() -> Self {
        Self::UNCATEGORIZED
    }
}

/// A byte count.
///
/// Addition is meaningful, so `Add`/`Sum` are implemented and saturate.
/// Multiplication and subtraction are not offered: a negative or scaled byte
/// count is always a bug at this layer.
///
/// [`Display`](fmt::Display) formats in **decimal SI**, because macOS Finder
/// does and a disagreeing number reads as a bug (docs/05-UI.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Bytes(u64);

impl Bytes {
    /// Zero bytes.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw byte count.
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// The raw byte count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether this is zero bytes.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Decimal SI rendering, e.g. `4.13 TB`. This is the default everywhere in
    /// the product.
    #[must_use]
    pub fn to_si(self) -> String {
        format_si(self.0)
    }

    /// Binary IEC rendering, e.g. `3.76 TiB`. Available behind a setting, and
    /// used for the memory budget, whose acceptance unit is `GiB`.
    #[must_use]
    pub fn to_iec(self) -> String {
        format_iec(self.0)
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_si(self.0))
    }
}

impl core::ops::Add for Bytes {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::AddAssign for Bytes {
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl core::iter::Sum for Bytes {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, core::ops::Add::add)
    }
}

impl From<u64> for Bytes {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Bytes> for u64 {
    fn from(value: Bytes) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_not_a_slot_and_not_a_group() {
        assert!(NodeId::NONE.is_none());
        assert!(NodeId::NONE.index().is_none());
        assert!(!NodeId::NONE.is_virtual_group());
        assert!(!NodeId::NONE.is_node());
    }

    #[test]
    fn index_ceiling_is_one_below_the_sentinel() {
        assert_eq!(
            NodeId::from_index(MAX_NODE_INDEX).and_then(NodeId::index),
            Some(MAX_NODE_INDEX)
        );
        assert!(NodeId::from_index(MAX_NODE_INDEX + 1).is_none());
        assert_eq!(NodeId::NONE.raw(), MAX_NODE_INDEX + 1);
    }

    #[test]
    fn virtual_groups_round_trip_and_stay_disjoint_from_slots() {
        let dir = NodeId::from_index(1234).expect("in range");
        let group = NodeId::virtual_group_of(dir).expect("dir is a slot");
        assert!(group.is_virtual_group());
        assert!(!group.is_node());
        assert!(group.index().is_none());
        assert_eq!(group.group_owner(), Some(dir));
        assert_eq!(NodeId::virtual_group_of(group), None);
        assert_eq!(NodeId::virtual_group_of(NodeId::NONE), None);
    }

    #[test]
    fn reserved_all_ones_is_neither_node_nor_group() {
        let reserved = NodeId::from_raw(u32::MAX);
        assert!(!reserved.is_node());
        assert!(!reserved.is_virtual_group());
        assert!(!reserved.is_valid());
        assert_eq!(reserved.group_owner(), None);
    }

    #[test]
    fn generations_and_scan_ids_saturate_rather_than_wrap() {
        assert_eq!(ScanId::from_raw(u64::MAX).next().get(), u64::MAX);
        assert_eq!(TreeGeneration::from_raw(u64::MAX).next().get(), u64::MAX);
        assert!(TreeGeneration::NONE.is_none());
    }

    #[test]
    fn bytes_add_saturates() {
        assert_eq!((Bytes::new(u64::MAX) + Bytes::new(9)).get(), u64::MAX);
        assert_eq!([Bytes::new(2), Bytes::new(3)].into_iter().sum::<Bytes>(), Bytes::new(5));
    }
}
