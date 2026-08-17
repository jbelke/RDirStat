//! Counting hard-linked content once per scan.
//!
//! For a regular file with `nlink > 1`, the **first** observed `(dev, ino)`
//! contributes logical and allocated bytes. Later directory entries stay
//! visible and carry
//! [`flags::HARD_LINK_REPEAT`](rdirstat_core::flags::HARD_LINK_REPEAT), which
//! makes [`Node::contributed_size`](rdirstat_core::Node::contributed_size)
//! return zero. Traversal order decides which path is the contributor, so the
//! UI says "counted at &lt;path&gt;" rather than implying that path owns the
//! bytes (docs/02-SCANNER.md#traversal-rules).
//!
//! Only entries with `nlink > 1` are inserted, so the set stays small even on a
//! 69M-entry volume.

use std::collections::HashSet;

/// The `(dev, ino)` pairs already counted in this scan.
#[derive(Debug, Default)]
pub struct HardLinkSet {
    seen: HashSet<(u64, u64)>,
}

impl HardLinkSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self { seen: HashSet::new() }
    }

    /// Records `(dev, ino)` and reports whether this is the first observation.
    ///
    /// `true` means "this entry contributes its bytes"; `false` means "this is
    /// a repeat and contributes zero".
    pub fn observe(&mut self, dev: u64, ino: u64) -> bool {
        self.seen.insert((dev, ino))
    }

    /// How many distinct hard-linked inodes have been counted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether nothing hard-linked has been observed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_observation_contributes_and_later_ones_do_not() {
        let mut set = HardLinkSet::new();
        assert!(set.observe(1, 42), "first sighting pays");
        assert!(!set.observe(1, 42), "second sighting is free");
        assert!(!set.observe(1, 42));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn the_same_inode_on_another_device_is_a_different_object() {
        let mut set = HardLinkSet::new();
        assert!(set.observe(1, 42));
        assert!(set.observe(2, 42), "inode numbers are only unique per device");
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
    }
}
