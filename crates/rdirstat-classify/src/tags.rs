//! Context tags: the *second* output of classification.
//!
//! A node has exactly one primary content category (colour, size statistics)
//! and zero or more context tags (reports). That split is what lets
//! `Library/Caches/movie.mp4` be a Video **and** be inside a Cache instead of
//! forcing a lossy choice (docs/04-CLASSIFICATION.md#two-outputs-not-one-overloaded-category).
//!
//! Tags are not stored per node. `Node` is 48 bytes and adding a tag bitset is
//! a memory-budget amendment, not a detail — so the builder carries the
//! inherited tag set down the path in its own descent state, and reports read
//! it from there.

use serde::{Deserialize, Serialize};

/// One context tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
#[non_exhaustive]
pub enum ContextTag {
    /// Inside a macOS bundle: `.app`, `.framework`, `.bundle`, ….
    Package = 0,
    /// Inside a photo, video, or music library bundle.
    MediaLibrary = 1,
    /// Inside a cache directory: `Caches`, `__pycache__`, `DerivedData`, ….
    Cache = 2,
    /// Inside build output: `.build`, `target`, `DerivedData`, `*.xcarchive`.
    BuildOutput = 3,
    /// Inside a fetched dependency tree: `node_modules`, `Pods`, `vendor`, ….
    DependencyTree = 4,
    /// Inside container or VM image storage.
    ContainerStorage = 5,
    /// Inside Apple bookkeeping: `.Spotlight-V100`, `.fseventsd`, ….
    AppleMetadata = 6,
    /// Inside a trash directory.
    Trash = 7,
}

impl ContextTag {
    /// Every tag, in declaration order. Used for iteration and for the
    /// configuration digest.
    pub const ALL: [Self; 8] = [
        Self::Package,
        Self::MediaLibrary,
        Self::Cache,
        Self::BuildOutput,
        Self::DependencyTree,
        Self::ContainerStorage,
        Self::AppleMetadata,
        Self::Trash,
    ];

    /// The stable, untranslated persistence key.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::MediaLibrary => "media_library",
            Self::Cache => "cache",
            Self::BuildOutput => "build_output",
            Self::DependencyTree => "dependency_tree",
            Self::ContainerStorage => "container_storage",
            Self::AppleMetadata => "apple_metadata",
            Self::Trash => "trash",
        }
    }

    /// The single bit this tag occupies in a [`ContextTags`] set.
    #[must_use]
    pub const fn bit(self) -> u16 {
        1u16 << (self as u8)
    }
}

/// A set of [`ContextTag`]s.
///
/// A 16-bit set, not a `Vec`: this is folded per directory during the scan and
/// must not allocate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ContextTags(u16);

impl ContextTags {
    /// The empty set.
    pub const NONE: Self = Self(0);

    /// A set from raw bits. Unknown bits are preserved, so a newer snapshot
    /// round-trips through an older build without silently losing tags.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// A set holding exactly one tag.
    #[must_use]
    pub const fn single(tag: ContextTag) -> Self {
        Self(tag.bit())
    }

    /// Whether `tag` is present.
    #[must_use]
    pub const fn contains(self, tag: ContextTag) -> bool {
        self.0 & tag.bit() != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The union of two sets. This is what the builder folds down a path.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The set with `tag` added.
    #[must_use]
    pub const fn with(self, tag: ContextTag) -> Self {
        Self(self.0 | tag.bit())
    }

    /// Iterates the known tags present in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = ContextTag> {
        ContextTag::ALL.into_iter().filter(move |tag| self.contains(*tag))
    }
}

impl FromIterator<ContextTag> for ContextTags {
    fn from_iter<I: IntoIterator<Item = ContextTag>>(iter: I) -> Self {
        iter.into_iter().fold(Self::NONE, Self::with)
    }
}

impl core::ops::BitOr for ContextTags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for ContextTags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

#[cfg(test)]
mod tests {
    use super::{ContextTag, ContextTags};

    #[test]
    fn bits_are_disjoint_and_stable() {
        let mut seen = 0u16;
        for tag in ContextTag::ALL {
            assert_eq!(seen & tag.bit(), 0, "{} reuses a bit", tag.key());
            seen |= tag.bit();
        }
        assert_eq!(seen, 0b1111_1111);
    }

    #[test]
    fn set_operations() {
        let tags = ContextTags::single(ContextTag::Cache).with(ContextTag::Trash);
        assert!(tags.contains(ContextTag::Cache));
        assert!(tags.contains(ContextTag::Trash));
        assert!(!tags.contains(ContextTag::Package));
        assert!(!tags.is_empty());
        assert!(ContextTags::NONE.is_empty());

        let merged = tags | ContextTags::single(ContextTag::Package);
        assert_eq!(merged.iter().count(), 3);
        assert_eq!(
            merged.iter().collect::<Vec<_>>(),
            vec![ContextTag::Package, ContextTag::Cache, ContextTag::Trash]
        );
    }

    #[test]
    fn unknown_bits_survive_a_round_trip() {
        let future = ContextTags::from_bits(0b1000_0000_0000_0000);
        assert_eq!(future.bits(), 0b1000_0000_0000_0000);
        assert_eq!(future.iter().count(), 0);
        assert!(!future.is_empty());
    }

    #[test]
    fn collects_from_an_iterator() {
        let tags: ContextTags = [ContextTag::Cache, ContextTag::Cache].into_iter().collect();
        assert_eq!(tags, ContextTags::single(ContextTag::Cache));
    }
}
