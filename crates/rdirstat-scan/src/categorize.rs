//! The seam between the scanner and the categorizer.
//!
//! `rdirstat-classify` owns the macOS taxonomy and the longest-suffix-first
//! matcher. The scanner only needs *a* function from name bytes to a
//! [`CategoryId`], so it takes this trait rather than a concrete type: that
//! keeps the scan crate independent of the classifier's shape and keeps the
//! differential tests runnable with no category table at all.

use rdirstat_core::{CategoryId, Kind};

/// Assigns a category to one entry.
///
/// Takes `&[u8]`, not `&str`: macOS names are NFD bytes and are not guaranteed
/// to be UTF-8. Implementations must not allocate per call.
pub trait Categorizer: Send + Sync {
    /// The category for an entry with this name, kind, and scan-time flags.
    fn categorize(&self, name: &[u8], kind: Kind, flags: u16) -> CategoryId;
}

/// The default: everything is [`CategoryId::UNCATEGORIZED`].
///
/// Used by tests, benchmarks, and any caller that has not wired a category
/// table. It is a real implementation, not a stub: "no category configuration
/// loaded" genuinely means every node is uncategorized.
#[derive(Clone, Copy, Debug, Default)]
pub struct Uncategorized;

impl Categorizer for Uncategorized {
    fn categorize(&self, _name: &[u8], _kind: Kind, _flags: u16) -> CategoryId {
        CategoryId::UNCATEGORIZED
    }
}

impl<T: Categorizer + ?Sized> Categorizer for &T {
    fn categorize(&self, name: &[u8], kind: Kind, flags: u16) -> CategoryId {
        (**self).categorize(name, kind, flags)
    }
}

/// Directory suffixes macOS users perceive as a single document.
///
/// The scanner still descends and accounts for the contents; only presentation
/// collapses them. Comparison is ASCII-case-insensitive because the volume may
/// be either.
pub const PACKAGE_SUFFIXES: [&[u8]; 18] = [
    b".app",
    b".appex",
    b".bundle",
    b".framework",
    b".kext",
    b".plugin",
    b".component",
    b".mdimporter",
    b".qlgenerator",
    b".prefpane",
    b".photoslibrary",
    b".aplibrary",
    b".imovielibrary",
    b".fcpbundle",
    b".tvlibrary",
    b".xcodeproj",
    b".xcworkspace",
    b".rtfd",
];

/// Whether a directory name looks like a macOS package.
#[must_use]
pub fn is_package_name(name: &[u8]) -> bool {
    PACKAGE_SUFFIXES.iter().any(|suffix| {
        name.len() > suffix.len() && {
            let tail = &name[name.len() - suffix.len()..];
            tail.eq_ignore_ascii_case(suffix)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_are_recognized_case_insensitively() {
        assert!(is_package_name(b"Safari.app"));
        assert!(is_package_name(b"Safari.APP"));
        assert!(is_package_name(b"Photos Library.photoslibrary"));
        assert!(is_package_name(b"rdirstat.xcodeproj"));
    }

    #[test]
    fn a_bare_suffix_or_an_unrelated_name_is_not_a_package() {
        assert!(!is_package_name(b".app"), "a dotfile named .app is not a package");
        assert!(!is_package_name(b"Applications"));
        assert!(!is_package_name(b"notes.txt"));
    }

    #[test]
    fn the_default_categorizer_returns_uncategorized() {
        let categorizer = Uncategorized;
        assert_eq!(
            categorizer.categorize(b"movie.mp4", Kind::File, 0),
            CategoryId::UNCATEGORIZED
        );
    }
}
