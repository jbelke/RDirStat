//! Where a backed-up item lands: the mirror layout.
//!
//! `<dest_root>/Backups/<source_label>/<path relative to the source volume
//! root>` (11-ORGANIZE.md, Amendment A). Naming and folder rules run later as
//! Organize on the copy, never during the backup.

use std::path::{Component, Path, PathBuf};

/// The directory under the destination root that holds every mirror.
pub const BACKUPS_DIR: &str = "Backups";

/// Longest accepted source label, in bytes.
pub const MAX_LABEL_LEN: usize = 64;

/// Why a mirror path was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// The label is empty, too long, starts with `.`, or uses a byte outside
    /// `[A-Za-z0-9._-]`.
    #[error("source label {0:?} is not allowed: use 1-64 of A-Z a-z 0-9 . _ - and do not start with '.'")]
    Label(String),
    /// The relative path is empty, absolute, or climbs with `..`.
    #[error("relative path {0:?} must be non-empty, relative, and free of '..'")]
    Relative(PathBuf),
}

/// Check a source label.
///
/// The accepted alphabet is deliberately narrow (`[A-Za-z0-9._-]`, not
/// starting with `.`, at most [`MAX_LABEL_LEN`] bytes). That rules out `/`,
/// `.`, `..`, NUL, and spellings that differ only by Unicode normalization,
/// which on a case- or normalization-insensitive destination could alias
/// two sources into one mirror.
///
/// # Errors
///
/// [`LayoutError::Label`] when the label is outside that alphabet.
pub fn validate_label(label: &str) -> Result<(), LayoutError> {
    let ok = !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && !label.starts_with('.')
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(LayoutError::Label(label.to_owned()))
    }
}

/// `dest_root/Backups/<source_label>/<relative>`.
///
/// `.` components in `relative` are dropped. The result is lexically inside
/// `dest_root/Backups/<source_label>`.
///
/// # Errors
///
/// [`LayoutError::Label`] for a bad label (see [`validate_label`]);
/// [`LayoutError::Relative`] when `relative` is empty after dropping `.`,
/// absolute, or contains `..`.
pub fn mirror_path(dest_root: &Path, source_label: &str, relative: &Path) -> Result<PathBuf, LayoutError> {
    validate_label(source_label)?;
    let mut out = dest_root.join(BACKUPS_DIR).join(source_label);
    let mut pushed = false;
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                pushed = true;
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(LayoutError::Relative(relative.to_path_buf()));
            }
        }
    }
    if !pushed {
        return Err(LayoutError::Relative(relative.to_path_buf()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_layout() {
        let p = mirror_path(
            Path::new("/Volumes/gdr22tb"),
            "mac-data",
            Path::new("Users/josh/Movies/a.mov"),
        );
        assert_eq!(
            p,
            Ok(PathBuf::from(
                "/Volumes/gdr22tb/Backups/mac-data/Users/josh/Movies/a.mov"
            ))
        );
        let q = mirror_path(Path::new("/d"), "tuf8tb", Path::new("./x/./y"));
        assert_eq!(q, Ok(PathBuf::from("/d/Backups/tuf8tb/x/y")));
    }

    #[test]
    fn bad_labels_are_refused() {
        for label in ["", ".", "..", "a/b", "../x", ".hidden", "a b", "é", "a\0b"] {
            assert!(
                matches!(
                    mirror_path(Path::new("/d"), label, Path::new("x")),
                    Err(LayoutError::Label(_))
                ),
                "{label:?}"
            );
        }
        let long = "a".repeat(MAX_LABEL_LEN + 1);
        assert!(validate_label(&long).is_err());
        assert!(validate_label(&"a".repeat(MAX_LABEL_LEN)).is_ok());
        assert!(validate_label("mac-data_2.v1").is_ok());
    }

    #[test]
    fn bad_relatives_are_refused() {
        for rel in ["", ".", "/etc/passwd", "a/../../b", ".."] {
            assert!(
                matches!(
                    mirror_path(Path::new("/d"), "mac", Path::new(rel)),
                    Err(LayoutError::Relative(_))
                ),
                "{rel:?}"
            );
        }
    }
}
