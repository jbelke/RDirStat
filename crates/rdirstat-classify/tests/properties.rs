//! Properties that must hold for *any* byte string a filesystem can produce.
//!
//! A scanner meets every hostile name the disk can invent — undecodable bytes,
//! 255-byte names, a hundred dots, a trailing dot, nothing but dots. None of
//! them may panic, and none may produce a category id that is not in the table.

#![allow(
    clippy::expect_used,
    reason = "test setup: a broken fixture must fail loudly, and clippy.toml only\n              exempts `expect` inside #[test] functions, not the helpers they call"
)]

use proptest::prelude::*;
use rdirstat_classify::{Categorizer, CategoryId};
use rdirstat_core::Kind;

fn categorizer() -> Categorizer {
    Categorizer::defaults().expect("the shipped defaults compile")
}

fn any_kind() -> impl Strategy<Value = Kind> {
    prop_oneof![
        Just(Kind::Unknown),
        Just(Kind::File),
        Just(Kind::Directory),
        Just(Kind::Symlink),
        Just(Kind::Socket),
        Just(Kind::Fifo),
        Just(Kind::CharDevice),
        Just(Kind::BlockDevice),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Total function: every byte string, every kind, every mode.
    #[test]
    fn classify_is_total(name in prop::collection::vec(any::<u8>(), 0..300), kind in any_kind(), mode in any::<u32>()) {
        let categorizer = categorizer();
        let id = categorizer.classify(&name, kind, mode);
        prop_assert!(categorizer.category(id).is_some(), "id {} is not in the table", id.get());
    }

    /// Names built from realistic dotted components, which is where the suffix
    /// walk actually does work.
    #[test]
    fn dotted_names_are_total(parts in prop::collection::vec("[.a-zA-Z0-9_-]{0,12}", 0..12)) {
        let categorizer = categorizer();
        let name = parts.join(".");
        let id = categorizer.classify(name.as_bytes(), Kind::File, 0o644);
        prop_assert!(categorizer.category(id).is_some());
        // The two byte- and str-shaped entry points must never disagree.
        prop_assert_eq!(id, categorizer.classify_str(&name, Kind::File, 0o644));
    }

    /// Rung 1 dominates the whole ladder.
    #[test]
    fn symlink_always_wins(name in prop::collection::vec(any::<u8>(), 0..120), mode in any::<u32>()) {
        let categorizer = categorizer();
        prop_assert_eq!(
            categorizer.classify(&name, Kind::Symlink, mode),
            categorizer.symlink_category()
        );
    }

    /// The execute bit is a *fallback*: it can only ever turn Uncategorized
    /// into Executable, never override a name match.
    #[test]
    fn execute_bit_only_fills_the_gap(name in prop::collection::vec(any::<u8>(), 0..120)) {
        let categorizer = categorizer();
        let plain = categorizer.classify(&name, Kind::File, 0o644);
        let executable = categorizer.classify(&name, Kind::File, 0o755);
        if plain == CategoryId::UNCATEGORIZED {
            prop_assert_eq!(executable, categorizer.executable_category());
        } else {
            prop_assert_eq!(executable, plain);
        }
    }

    /// A longer suffix cannot be *skipped*: appending a matching extension to
    /// any stem must produce that extension's category, whatever the stem is.
    #[test]
    // A non-empty stem: `.mp4` on its own is a dotfile, not an extension.
    fn appending_a_known_extension_decides_the_category(stem in "[a-zA-Z0-9 _()-]{1,40}") {
        let categorizer = categorizer();
        for (extension, key) in [("mp4", "video"), ("tar.gz", "compressed-archive"), ("gz", "compressed-stream")] {
            let name = format!("{stem}.{extension}");
            let id = categorizer.classify(name.as_bytes(), Kind::File, 0o644);
            prop_assert_eq!(categorizer.key_of(id), Some(key), "{}", name);
        }
    }

    /// Directory-only categories never leak onto files and vice versa.
    #[test]
    fn directory_eligibility_holds(name in "[a-zA-Z0-9._-]{1,40}") {
        let categorizer = categorizer();
        let id = categorizer.classify(name.as_bytes(), Kind::Directory, 0o755);
        if let Some(category) = categorizer.category(id) {
            prop_assert!(
                category.directory_eligible() || id == CategoryId::UNCATEGORIZED,
                "directory {} got file-only category {}", name, category.key()
            );
        }
    }

    /// Context tagging is component-aware: a component that merely *contains* a
    /// tagged name must not inherit its tags.
    #[test]
    fn context_tags_do_not_match_substrings(prefix in "[a-zA-Z]{1,8}", suffix in "[a-zA-Z]{1,8}") {
        let categorizer = categorizer();
        let name = format!("{prefix}node_modules{suffix}");
        prop_assert!(categorizer.context_tags(name.as_bytes()).is_empty(), "{}", name);
    }
}
