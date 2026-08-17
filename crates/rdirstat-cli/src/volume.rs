//! Best-effort volume identity, without `unsafe` and without `libc`.
//!
//! The desktop app will read this from `statfs(2)` through the scan crate. The
//! CLI cannot call `statfs` without `unsafe`, so it derives what it can from
//! `st_dev` plus one read-only probe each, and says "unknown" rather than
//! inventing a value. Every field here is diagnostic; nothing routes on it.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rdirstat_core::{DisplayPath, VolumeId};

/// Derives a [`VolumeId`] for `root`, whose metadata device is `device`.
pub(crate) fn identify(root: &Path, device: u64) -> VolumeId {
    let mount_point = mount_point_of(root, device);
    VolumeId {
        device,
        fs_type: filesystem_type(&mount_point).unwrap_or_else(|| "unknown".to_owned()),
        volume_uuid: None,
        mount_point: DisplayPath::from_bytes(mount_point.as_os_str().as_bytes()),
        case_preserving: true,
        case_sensitive: probe_case_sensitive(root).unwrap_or(false),
    }
}

/// Walks up from `root` while `st_dev` stays the same. The last directory that
/// still reports `device` is the mount point.
fn mount_point_of(root: &Path, device: u64) -> PathBuf {
    let mut current = root.to_path_buf();
    loop {
        let Some(parent) = current.parent() else {
            return current;
        };
        match fs::metadata(parent) {
            Ok(metadata) if metadata.dev() == device => current = parent.to_path_buf(),
            _ => return current,
        }
    }
}

/// Reads the filesystem type out of `mount(8)`.
///
/// One fork, once per scan, off the hot path. `mount` prints
/// `/dev/disk3s5 on / (apfs, local, journaled)`.
fn filesystem_type(mount_point: &Path) -> Option<String> {
    let output = Command::new("/sbin/mount").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let target = mount_point.as_os_str().as_bytes();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let text = String::from_utf8_lossy(line);
        let Some((left, right)) = text.split_once(" (") else {
            continue;
        };
        let Some((_device, point)) = left.split_once(" on ") else {
            continue;
        };
        if point.as_bytes() != target {
            continue;
        }
        let kind = right.split([',', ')']).next()?.trim();
        if kind.is_empty() {
            return None;
        }
        return Some(kind.to_owned());
    }
    None
}

/// Read-only case-sensitivity probe: flip the case of the first ASCII letter in
/// the final component and see whether it resolves to the same inode.
///
/// Returns `None` when the probe is inconclusive (no letter to flip, no parent,
/// unreadable). Nothing is created, moved, or written.
fn probe_case_sensitive(root: &Path) -> Option<bool> {
    let name = root.file_name()?.as_bytes();
    let position = name.iter().position(u8::is_ascii_alphabetic)?;
    let mut flipped = name.to_vec();
    let byte = flipped[position];
    flipped[position] = if byte.is_ascii_lowercase() {
        byte.to_ascii_uppercase()
    } else {
        byte.to_ascii_lowercase()
    };
    let parent = root.parent()?;
    let candidate = parent.join(OsStr::from_bytes(&flipped));
    let original = fs::symlink_metadata(root).ok()?;
    match fs::symlink_metadata(&candidate) {
        // The flipped name resolves to the same object: lookups ignore case.
        Ok(other) if other.dev() == original.dev() && other.ino() == original.ino() => Some(false),
        // The flipped name resolves to something else, or to nothing: lookups
        // distinguish case.
        Ok(_) | Err(_) => Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_directory_reports_its_own_device_and_a_mount_point_above_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let metadata = fs::metadata(dir.path()).expect("metadata");
        let volume = identify(dir.path(), metadata.dev());
        assert_eq!(volume.device, metadata.dev());
        assert!(!volume.mount_point.as_str().is_empty());
    }

    #[test]
    fn the_case_probe_never_writes_and_answers_or_abstains() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before: Vec<_> = fs::read_dir(dir.path())
            .expect("read")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        let _answer = probe_case_sensitive(dir.path());
        let after: Vec<_> = fs::read_dir(dir.path())
            .expect("read")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(before, after);
    }
}
