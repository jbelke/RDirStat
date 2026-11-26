//! Whether a source and a destination make a backup at all.
//!
//! A copy onto the same physical disk is not a backup: one failed drive takes
//! both. The facts come from the shell (`rdirstat-copy::disk_identity`); this
//! module only judges them.

use serde::{Deserialize, Serialize};

/// What the shell knows about the disk under one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskFacts {
    /// Physical store identifier (for example `disk14`), `None` when unknown.
    pub disk_id: Option<String>,
    /// `st_dev` of the mounted filesystem.
    pub st_dev: u64,
    /// Filesystem type name as reported by the OS (`apfs`, `exfat`, ...).
    pub filesystem: String,
    /// Capacity of the volume in bytes.
    pub total_bytes: u64,
    /// Bytes available to the user.
    pub available_bytes: u64,
}

impl DiskFacts {
    /// Bytes in use, `total - available`, never below zero.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }

    fn known_disk_id(&self) -> Option<&str> {
        self.disk_id.as_deref().map(str::trim).filter(|id| !id.is_empty())
    }
}

/// Why a source/destination pair is refused as a backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum PairRefusal {
    /// Both paths live on one physical disk.
    #[error("source and destination are on the same physical disk; that is not a backup")]
    SameDisk,
    /// The physical disk of one side could not be determined.
    #[error("the physical disk of the source or destination is unknown; refusing to guess")]
    UnknownDisk,
    /// Both paths are on one mounted filesystem.
    #[error("source and destination are on the same filesystem; that is not a backup")]
    SameFilesystem,
}

/// Accept the pair only when both disks are known and differ.
///
/// Order of checks: one filesystem (`st_dev` equal) is refused first, because
/// that needs no disk lookup to be true; then an unknown (missing or blank)
/// `disk_id` on either side; then equal `disk_id`s.
///
/// # Errors
///
/// The [`PairRefusal`] naming the first check that failed.
pub fn judge_backup_pair(source: &DiskFacts, dest: &DiskFacts) -> Result<(), PairRefusal> {
    if source.st_dev == dest.st_dev {
        return Err(PairRefusal::SameFilesystem);
    }
    match (source.known_disk_id(), dest.known_disk_id()) {
        (Some(a), Some(b)) if a == b => Err(PairRefusal::SameDisk),
        (Some(_), Some(_)) => Ok(()),
        _ => Err(PairRefusal::UnknownDisk),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(disk: Option<&str>, dev: u64) -> DiskFacts {
        DiskFacts {
            disk_id: disk.map(str::to_owned),
            st_dev: dev,
            filesystem: "apfs".into(),
            total_bytes: 100,
            available_bytes: 40,
        }
    }

    #[test]
    fn different_disks_are_accepted() {
        assert_eq!(
            judge_backup_pair(&facts(Some("disk3"), 1), &facts(Some("disk14"), 2)),
            Ok(())
        );
    }

    #[test]
    fn same_disk_is_refused() {
        assert_eq!(
            judge_backup_pair(&facts(Some("disk3"), 1), &facts(Some("disk3"), 2)),
            Err(PairRefusal::SameDisk)
        );
    }

    #[test]
    fn missing_disk_id_is_refused() {
        for (a, b) in [
            (None, Some("disk14")),
            (Some("disk3"), None),
            (None, None),
            (Some(""), Some("disk14")),
            (Some("disk3"), Some("  ")),
        ] {
            assert_eq!(
                judge_backup_pair(&facts(a, 1), &facts(b, 2)),
                Err(PairRefusal::UnknownDisk),
                "{a:?} {b:?}"
            );
        }
    }

    #[test]
    fn same_filesystem_is_refused() {
        assert_eq!(
            judge_backup_pair(&facts(Some("disk3"), 7), &facts(Some("disk14"), 7)),
            Err(PairRefusal::SameFilesystem)
        );
    }

    #[test]
    fn used_bytes_saturates() {
        let mut f = facts(None, 1);
        assert_eq!(f.used_bytes(), 60);
        f.available_bytes = 500;
        assert_eq!(f.used_bytes(), 0);
    }
}
