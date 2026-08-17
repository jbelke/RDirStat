//! End-to-end classification over a real directory tree.
//!
//! The unit tests classify byte literals; this one classifies whatever the
//! filesystem actually hands back, which is the only way to catch a wrong
//! assumption about `OsStr` bytes, the execute bit, or symlink detection.
//!
//! Every path here lives inside a `TempDir`. Nothing is written anywhere else
//! (docs/08-RUST-PRACTICES.md#testing).

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "fixture setup: a broken fixture must fail loudly, and clippy.toml only\n              exempts `expect` inside #[test] functions, not the helpers they call"
)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use rdirstat_classify::{Categorizer, ContextTag};
use rdirstat_core::Kind;
use tempfile::TempDir;

/// Maps a real `std::fs` entry onto the contract's [`Kind`].
fn kind_of(metadata: &fs::Metadata) -> Kind {
    use std::os::unix::fs::FileTypeExt;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_dir() {
        Kind::Directory
    } else if file_type.is_file() {
        Kind::File
    } else if file_type.is_socket() {
        Kind::Socket
    } else if file_type.is_fifo() {
        Kind::Fifo
    } else if file_type.is_char_device() {
        Kind::CharDevice
    } else if file_type.is_block_device() {
        Kind::BlockDevice
    } else {
        Kind::Unknown
    }
}

/// Classifies every direct child of `root`, keyed by name.
fn classify_children(categorizer: &Categorizer, root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(root).expect("the fixture directory is readable") {
        let entry = entry.expect("a readable directory entry");
        // `symlink_metadata` never follows: a symlink must classify as itself.
        let metadata = entry.path().symlink_metadata().expect("metadata");
        let name = entry.file_name();
        let id = categorizer.classify(name.as_bytes(), kind_of(&metadata), metadata.permissions().mode());
        out.insert(
            name.to_string_lossy().into_owned(),
            categorizer.key_of(id).unwrap_or("<unknown>").to_owned(),
        );
    }
    out
}

/// A name that is not valid UTF-8.
///
/// APFS and HFS+ *validate* this and return `EILSEQ`, so on a stock macOS
/// `TempDir` the file cannot be created at all. That is a property of the
/// volume, not of the classifier: an exFAT stick, an SMB share or a Linux
/// container bind mount will hand back exactly these bytes. The fixture tries,
/// records whether it worked, and asserts the byte path either way.
const NON_UTF8_NAME: &[u8] = b"broken\xff\xfename.mov";

/// A fixture tree plus what the underlying volume allowed.
struct Fixture {
    temp: TempDir,
    non_utf8_name_created: bool,
}

impl Fixture {
    fn path(&self) -> &Path {
        self.temp.path()
    }
}

fn build_fixture() -> Fixture {
    let temp = TempDir::new().expect("a temporary directory");
    let root = temp.path();

    for (name, contents) in [
        ("Photo.JPG", "not really a jpeg"),
        ("backup.tar.gz", "not really an archive"),
        ("old.tar.Z", "not really compress(1)"),
        ("notes.txt", "notes"),
        ("main.C", "// C++"),
        // NOT "main.c": a TempDir on APFS is case-INSENSITIVE by default, so
        // `main.c` would silently overwrite `main.C` and the fixture would lie.
        ("other.c", "/* C */"),
        (".DS_Store", "finder junk"),
        ("._Sidecar", "appledouble twin"),
        (".gitignore", "target/"),
        ("README", "readme"),
        ("Docker.raw", "container disk"),
        ("disk.qcow2", "vm disk"),
        ("mystery", "no extension, not executable"),
    ] {
        fs::write(root.join(name), contents).expect("write fixture file");
    }

    // A file that is only an Executable because of its mode bits.
    let script = root.join("configure");
    fs::write(&script, "#!/bin/sh\n").expect("write fixture file");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    // A name with an extension AND the execute bit: the name must win.
    let named = root.join("build.sh");
    fs::write(&named, "#!/bin/sh\n").expect("write fixture file");
    fs::set_permissions(&named, fs::Permissions::from_mode(0o755)).expect("chmod");

    let raw_name = std::ffi::OsStr::from_bytes(NON_UTF8_NAME);
    let non_utf8_name_created = fs::write(root.join(raw_name), "bytes").is_ok();

    for directory in ["node_modules", "Safari.app", "Photos.photoslibrary", "Documents"] {
        fs::create_dir(root.join(directory)).expect("create fixture directory");
    }

    symlink(root.join("Photo.JPG"), root.join("link-to-photo.JPG")).expect("symlink");
    symlink(root.join("nowhere"), root.join("broken.mp4")).expect("dangling symlink");

    Fixture {
        temp,
        non_utf8_name_created,
    }
}

#[test]
fn a_real_tree_classifies_the_way_the_ladder_says_it_should() {
    let categorizer = Categorizer::defaults().expect("defaults compile");
    let fixture = build_fixture();
    let observed = classify_children(&categorizer, fixture.path());

    let expected: &[(&str, &str)] = &[
        ("Photo.JPG", "image"),
        ("backup.tar.gz", "compressed-archive"),
        ("old.tar.Z", "compressed-archive"),
        ("notes.txt", "document"),
        ("main.C", "source"),
        ("other.c", "source"),
        (".DS_Store", "apple-metadata"),
        ("._Sidecar", "apple-metadata"),
        (".gitignore", "source"),
        ("README", "uncategorized"),
        ("Docker.raw", "container-disk"),
        ("disk.qcow2", "vm-disk"),
        ("mystery", "uncategorized"),
        ("configure", "executable"),
        ("build.sh", "source"),
        ("node_modules", "cache"),
        ("Safari.app", "package"),
        ("Photos.photoslibrary", "media-library"),
        ("Documents", "uncategorized"),
        // Both symlinks, including the dangling one, and including the one
        // whose name ends in a media extension.
        ("link-to-photo.JPG", "symlink"),
        ("broken.mp4", "symlink"),
    ];

    for (name, key) in expected {
        assert_eq!(observed.get(*name).map(String::as_str), Some(*key), "{name}");
    }

    // Undecodable bytes classify by suffix, whether or not this volume would
    // store such a name.
    assert_eq!(
        categorizer.key_of(categorizer.classify(NON_UTF8_NAME, Kind::File, 0o644)),
        Some("video")
    );
    let mut expected_entries = expected.len();
    if fixture.non_utf8_name_created {
        let lossy = String::from_utf8_lossy(NON_UTF8_NAME).into_owned();
        assert_eq!(observed.get(&lossy).map(String::as_str), Some("video"));
        expected_entries += 1;
    }

    assert_eq!(observed.len(), expected_entries, "fixture and expectations disagree");
}

#[test]
fn classification_is_stable_across_two_passes_of_the_same_tree() {
    let categorizer = Categorizer::defaults().expect("defaults compile");
    let fixture = build_fixture();
    let first = classify_children(&categorizer, fixture.path());
    let second = classify_children(&categorizer, fixture.path());
    assert_eq!(first, second);

    // ...and across an independently compiled categorizer with the same config,
    // which is what makes a stored `CategoryId` meaningful between scans.
    let other = Categorizer::defaults().expect("defaults compile");
    assert_eq!(other.digest(), categorizer.digest());
    assert_eq!(classify_children(&other, fixture.path()), first);
}

#[test]
fn context_tags_come_from_real_directory_components() {
    let categorizer = Categorizer::defaults().expect("defaults compile");
    let fixture = build_fixture();
    let nested = fixture.path().join("node_modules").join("Caches");
    fs::create_dir_all(&nested).expect("create nested fixture directories");

    // What the builder does: fold each component's tags as it descends.
    let mut inherited = rdirstat_classify::ContextTags::NONE;
    for component in nested
        .strip_prefix(fixture.path())
        .expect("nested is under the fixture root")
        .components()
    {
        inherited |= categorizer.context_tags(component.as_os_str().as_bytes());
    }
    assert!(inherited.contains(ContextTag::DependencyTree));
    assert!(inherited.contains(ContextTag::Cache));
    assert!(!inherited.contains(ContextTag::Trash));
}
