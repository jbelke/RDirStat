//! Which physical disk a path lives on.
//!
//! A backup refuses a destination on the same physical disk as its source, so
//! `disk_id` names the *physical* store, not the APFS container: a Mac's
//! containers are synthesized disks (`disk3`) that all sit on one SSD
//! (`disk0`). The lookup follows the same route as `src-tauri/src/volumes.rs`
//! does for `VolumeInfo.disk_id`: volume → `APFSContainerReference` (or
//! `ParentWholeDisk`) → the container's `APFSPhysicalStore` → its whole disk.

use std::ffi::CStr;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{CopyError, c_path, run};

/// The facts a backup pair is judged on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskIdentity {
    /// Physical whole disk, e.g. `disk14`. `None` when `diskutil` cannot say;
    /// callers treat unknown as "refuse", never as "different".
    pub disk_id: Option<String>,
    /// `statfs` type name: `apfs`, `exfat`, `hfs`, `smbfs`, …
    pub filesystem: String,
    /// `st_dev` of `path` itself.
    pub st_dev: u64,
    /// `pathconf(_PC_CASE_SENSITIVE)`; `None` when the filesystem won't say.
    pub case_sensitive: Option<bool>,
    pub total_bytes: u64,
    /// Bytes available to an unprivileged writer (`f_bavail`).
    pub available_bytes: u64,
    pub mount_point: PathBuf,
}

/// Identifies the volume and physical disk holding `path`.
///
/// # Errors
///
/// [`CopyError::Io`] if `path` cannot be stat'ed. A failing `diskutil` is not
/// an error: it degrades `disk_id` to `None`.
pub fn disk_identity(path: &Path) -> Result<DiskIdentity, CopyError> {
    let st_dev = std::fs::metadata(path)
        .map_err(|error| CopyError::io(path, error))?
        .dev();
    let c = c_path(path)?;
    let fs = statfs_of(&c).map_err(|error| CopyError::io(path, error))?;
    let disk_id = if fs.mounted_from.starts_with("/dev/") {
        physical_disk_of(&fs.mounted_from)
    } else {
        None
    };
    Ok(DiskIdentity {
        disk_id,
        filesystem: fs.filesystem,
        st_dev,
        case_sensitive: case_sensitive(&c),
        total_bytes: fs.total_bytes,
        available_bytes: fs.available_bytes,
        mount_point: fs.mount_point,
    })
}

struct StatFs {
    filesystem: String,
    mount_point: PathBuf,
    mounted_from: String,
    total_bytes: u64,
    available_bytes: u64,
}

fn c_chars(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[allow(unsafe_code)]
fn statfs_of(path: &CStr) -> io::Result<StatFs> {
    // SAFETY: an all-zero `statfs` is a valid value of a plain C struct of
    // integers and char arrays; `path` is NUL-terminated and outlives the
    // call; `buf` is a live, exclusively borrowed `statfs` the kernel fills.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: see above.
    let result = unsafe { libc::statfs(path.as_ptr(), &raw mut buf) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let block = u64::from(buf.f_bsize);
    Ok(StatFs {
        filesystem: c_chars(&buf.f_fstypename),
        mount_point: PathBuf::from(c_chars(&buf.f_mntonname)),
        mounted_from: c_chars(&buf.f_mntfromname),
        total_bytes: buf.f_blocks.saturating_mul(block),
        available_bytes: buf.f_bavail.saturating_mul(block),
    })
}

#[allow(unsafe_code)]
fn case_sensitive(path: &CStr) -> Option<bool> {
    // SAFETY: `path` is NUL-terminated and outlives the call; pathconf only
    // reads it.
    let value = unsafe { libc::pathconf(path.as_ptr(), libc::_PC_CASE_SENSITIVE) };
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn diskutil_info(target: &str) -> Option<String> {
    let stdout = run(
        "diskutil",
        Command::new("/usr/sbin/diskutil").args(["info", "-plist", target]),
    )
    .ok()?;
    Some(String::from_utf8_lossy(&stdout).into_owned())
}

/// `/dev/disk3s5` → `disk0` (APFS physical store), or the partition's whole
/// disk for a plain scheme. `None` when `diskutil` cannot answer.
fn physical_disk_of(device_node: &str) -> Option<String> {
    let volume = diskutil_info(device_node)?;
    let container = plist_string(&volume, "APFSContainerReference")
        .or_else(|| plist_string(&volume, "ParentWholeDisk"))
        .filter(|id| !id.is_empty())?;
    let Some(info) = diskutil_info(&container) else {
        return Some(container);
    };
    plist_string(&info, "APFSPhysicalStore")
        .as_deref()
        .and_then(whole_disk_of)
        .or_else(|| plist_string(&info, "ParentWholeDisk"))
        .filter(|id| !id.is_empty())
        .or(Some(container))
}

fn plist_string(plist: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let at = plist.find(&needle)?;
    let rest = plist.get(at + needle.len()..)?.trim_start();
    let body = rest.strip_prefix("<string>")?;
    let end = body.find("</string>")?;
    body.get(..end).map(str::to_owned)
}

/// `disk0s2` → `disk0`; anything not shaped `diskN…` → `None`.
fn whole_disk_of(slice: &str) -> Option<String> {
    let digits: String = slice
        .strip_prefix("disk")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    Some(format!("disk{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_resolve_to_whole_disks() {
        assert_eq!(whole_disk_of("disk0s2").as_deref(), Some("disk0"));
        assert_eq!(whole_disk_of("disk14s1").as_deref(), Some("disk14"));
        assert_eq!(whole_disk_of("disk7").as_deref(), Some("disk7"));
        assert_eq!(whole_disk_of("/dev/disk0s2"), None);
        assert_eq!(whole_disk_of("md0"), None);
    }

    #[test]
    fn plist_strings_read_or_degrade() {
        let plist = "<dict><key>APFSPhysicalStore</key>\n\t<string>disk0s2</string></dict>";
        assert_eq!(plist_string(plist, "APFSPhysicalStore").as_deref(), Some("disk0s2"));
        assert_eq!(plist_string(plist, "Missing"), None);
    }

    #[test]
    fn a_temp_dir_has_a_coherent_identity() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let identity = disk_identity(scratch.path()).expect("identity");
        assert!(identity.total_bytes > 0);
        assert!(identity.available_bytes <= identity.total_bytes);
        assert!(!identity.filesystem.is_empty());
        // Not `starts_with`: `/private/var` is a firmlink into
        // `/System/Volumes/Data`, the mount `statfs` reports.
        assert!(identity.mount_point.is_absolute(), "{identity:?}");
        assert_eq!(identity.st_dev, std::fs::metadata(scratch.path()).expect("meta").dev());
        if let Some(id) = &identity.disk_id {
            assert_eq!(
                whole_disk_of(id).as_deref(),
                Some(id.as_str()),
                "a physical id has no slice suffix"
            );
        }
    }

    #[test]
    fn a_missing_path_is_an_error() {
        assert!(disk_identity(Path::new("/nonexistent/rdirstat/x")).is_err());
    }
}
