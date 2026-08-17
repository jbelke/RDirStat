//! The volume picker's data source.
//!
//! # Why this shells out
//!
//! `statfs(2)` / `getmntinfo(3)` are the right calls, and both require
//! `unsafe extern` FFI. The workspace denies `unsafe_code` everywhere except
//! `rdirstat-scan::sys::bulk`, and this crate is not that exception, so the
//! same `statfs` data is read through `/bin/df -k -l -Y` — which is a thin
//! wrapper over exactly those syscalls — and the device number comes from
//! `std::fs::metadata().dev()`, which is safe.
//!
//! Replace this module with a `statfs` binding the moment a crate is allowed to
//! own one; every caller goes through [`list`], [`fs_type_at`], and
//! [`mount_point_at`].
//!
//! Volume capacity is reported **beside** a scan's tree total and never
//! reconciled into it: clones, snapshots, purgeable space, exclusions,
//! unreadable data, and concurrent mutation are all legitimate deltas.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use rdirstat_core::{DisplayPath, VolumeInfo};

/// One parsed `df` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountEntry {
    /// The `df` "Filesystem" column, e.g. `/dev/disk3s1s1`.
    pub source: String,
    /// The `df` "Type" column, e.g. `apfs`.
    pub fs_type: String,
    /// Total capacity in bytes.
    pub total_bytes: u64,
    /// Available capacity in bytes.
    pub available_bytes: u64,
    /// Where it is mounted.
    pub mount_point: PathBuf,
}

/// Parses `df -k -l -Y` output.
///
/// The mount point is the **rest of the line** after the fixed columns, because
/// a volume name may contain spaces (`/Volumes/Time Machine`). Rows that do not
/// have the expected column count are skipped rather than mis-parsed.
fn parse_df(text: &str) -> Vec<MountEntry> {
    // Filesystem Type 1024-blocks Used Available | Capacity iused ifree %iused | Mounted-on
    // Five columns are read by name; four more are skipped; the rest is the path.
    const SKIPPED_COLUMNS: usize = 4;
    let mut entries = Vec::new();
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(source) = fields.next() else { continue };
        let Some(fs_type) = fields.next() else { continue };
        let Some(blocks) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let Some(_used) = fields.next() else { continue };
        let Some(available) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        // Skip Capacity, iused, ifree, %iused; whatever is left is the path,
        // which may contain spaces (`/Volumes/Time Machine`).
        let mut short = false;
        for _ in 0..SKIPPED_COLUMNS {
            if fields.next().is_none() {
                short = true;
                break;
            }
        }
        let rest: Vec<&str> = fields.collect();
        if short || rest.is_empty() {
            continue;
        }
        entries.push(MountEntry {
            source: source.to_owned(),
            fs_type: fs_type.to_owned(),
            total_bytes: blocks.saturating_mul(1_024),
            available_bytes: available.saturating_mul(1_024),
            mount_point: PathBuf::from(rest.join(" ")),
        });
    }
    entries
}

fn read_mounts() -> Vec<MountEntry> {
    let Ok(output) = Command::new("/bin/df").args(["-k", "-l", "-Y"]).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_df(&String::from_utf8_lossy(&output.stdout))
}

/// The mount entry whose mount point is the longest prefix of `path`.
fn containing_mount(mounts: &[MountEntry], path: &Path) -> Option<MountEntry> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.mount_point))
        .max_by_key(|mount| mount.mount_point.as_os_str().len())
        .cloned()
}

/// The filesystem type at `path`, e.g. `apfs`.
#[must_use]
pub(crate) fn fs_type_at(path: &Path) -> Option<String> {
    containing_mount(&read_mounts(), path).map(|mount| mount.fs_type)
}

/// The mount point containing `path`.
#[must_use]
pub(crate) fn mount_point_at(path: &Path) -> Option<PathBuf> {
    containing_mount(&read_mounts(), path).map(|mount| mount.mount_point)
}

/// Whether Time Machine local snapshots exist on `mount_point`.
///
/// v1 reports **presence only**. `tmutil` gives no authoritative byte total, so
/// no number here is ever subtracted from capacity.
fn has_local_snapshots(mount_point: &Path) -> bool {
    Command::new("/usr/bin/tmutil")
        .arg("listlocalsnapshots")
        .arg(mount_point)
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("com.apple.TimeMachine")
        })
}

/// Whether the backing device is removable, per `diskutil`.
fn is_removable(source: &str) -> bool {
    if !source.starts_with("/dev/") {
        return false;
    }
    Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", source])
        .output()
        .is_ok_and(|output| {
            if !output.status.success() {
                return false;
            }
            let plist = String::from_utf8_lossy(&output.stdout);
            plist_bool(&plist, "Removable") || plist_bool(&plist, "RemovableMediaOrExternalDevice")
        })
}

/// Reads `<key>NAME</key><true/>` out of a `diskutil -plist` document.
///
/// A full plist parser is not a dependency this crate needs for two booleans;
/// the shape is fixed and a parse miss degrades to `false`, never to a wrong
/// `true`.
fn plist_bool(plist: &str, key: &str) -> bool {
    let needle = format!("<key>{key}</key>");
    plist
        .find(&needle)
        .and_then(|at| plist.get(at + needle.len()..))
        .is_some_and(|rest| rest.trim_start().starts_with("<true/>"))
}

/// Reads `<key>NAME</key><string>VALUE</string>`.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let at = plist.find(&needle)?;
    let rest = plist.get(at + needle.len()..)?.trim_start();
    let open = "<string>";
    let body = rest.strip_prefix(open)?;
    let end = body.find("</string>")?;
    body.get(..end).map(str::to_owned)
}

fn volume_name(source: &str, mount_point: &Path) -> String {
    let from_diskutil = Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", source])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| plist_string(&String::from_utf8_lossy(&output.stdout), "VolumeName"));
    from_diskutil
        .filter(|name| !name.is_empty())
        .or_else(|| mount_point.file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_else(|| mount_point.to_string_lossy().into_owned())
}

/// Lists mounted local volumes for the launch screen.
///
/// Blocking: it runs `df`, and `diskutil`/`tmutil` once per volume. Always call
/// it from `spawn_blocking`.
#[must_use]
pub(crate) fn list() -> Vec<VolumeInfo> {
    let root_device = std::fs::metadata("/").map(|metadata| metadata.dev()).ok();
    read_mounts()
        .into_iter()
        .filter_map(|mount| {
            let device = std::fs::metadata(&mount.mount_point)
                .map(|metadata| metadata.dev())
                .ok()?;
            Some(VolumeInfo {
                name: volume_name(&mount.source, &mount.mount_point),
                mount_point: DisplayPath::from_bytes(mount.mount_point.as_os_str().as_encoded_bytes()),
                device,
                fs_type: mount.fs_type.clone(),
                total_bytes: mount.total_bytes,
                available_bytes: mount.available_bytes,
                // `volumeAvailableCapacityForImportantUsage` is a Foundation
                // resource value and needs an ObjC bridge; reporting `None` is
                // honest, reporting `available_bytes` twice would not be.
                important_available_bytes: None,
                is_root_volume: root_device == Some(device),
                is_removable: is_removable(&mount.source),
                has_local_snapshots: has_local_snapshots(&mount.mount_point),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Filesystem   Type  1024-blocks      Used Available Capacity iused      ifree %iused  Mounted on
/dev/disk3s1s1 apfs   971350180  10222448 145513744     7%  404052 1455137440    0%   /
devfs        devfs          200       200         0   100%     692          0  100%   /dev
/dev/disk3s5 apfs     971350180 800000000 145513744    85% 3000000 1455137440    1%   /System/Volumes/Data
/dev/disk8s1 exfat      1953100    100000   1853100     6%       0          0    0%   /Volumes/Time Machine
";

    #[test]
    fn df_rows_parse_including_mount_points_with_spaces() {
        let entries = parse_df(SAMPLE);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].fs_type, "apfs");
        assert_eq!(entries[0].mount_point, Path::new("/"));
        assert_eq!(entries[0].total_bytes, 971_350_180 * 1_024);
        assert_eq!(entries[0].available_bytes, 145_513_744 * 1_024);
        assert_eq!(entries[3].mount_point, Path::new("/Volumes/Time Machine"));
        assert_eq!(entries[3].fs_type, "exfat");
    }

    #[test]
    fn a_malformed_row_is_skipped_not_mis_parsed() {
        let entries = parse_df("Header\nnot enough columns\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn the_longest_matching_mount_point_wins() {
        let entries = parse_df(SAMPLE);
        let found = containing_mount(&entries, Path::new("/System/Volumes/Data/Users/nobody")).expect("a mount");
        assert_eq!(found.mount_point, Path::new("/System/Volumes/Data"));
        let root = containing_mount(&entries, Path::new("/Applications")).expect("a mount");
        assert_eq!(root.mount_point, Path::new("/"));
    }

    #[test]
    fn plist_scalars_read_or_degrade_to_none() {
        let plist = "<dict><key>Removable</key><true/><key>VolumeName</key><string>Macintosh HD</string></dict>";
        assert!(plist_bool(plist, "Removable"));
        assert!(!plist_bool(plist, "Ejectable"));
        assert_eq!(plist_string(plist, "VolumeName").as_deref(), Some("Macintosh HD"));
        assert_eq!(plist_string(plist, "Missing"), None);
    }

    #[test]
    fn the_running_machine_reports_at_least_a_root_volume() {
        let volumes = list();
        assert!(!volumes.is_empty(), "df must report at least `/`");
        assert!(
            volumes.iter().any(|volume| volume.is_root_volume),
            "exactly one volume is the boot volume"
        );
        assert!(
            volumes
                .iter()
                .all(|volume| volume.total_bytes > 0 || volume.fs_type == "devfs")
        );
    }

    #[test]
    fn the_filesystem_type_of_the_boot_volume_is_known() {
        assert!(fs_type_at(Path::new("/")).is_some());
        assert_eq!(mount_point_at(Path::new("/")).as_deref(), Some(Path::new("/")));
    }
}
