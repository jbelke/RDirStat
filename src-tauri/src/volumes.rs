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
//! own one; every caller goes through [`list`].
//!
//! Volume capacity is reported **beside** a scan's tree total and never
//! reconciled into it: clones, snapshots, purgeable space, exclusions,
//! unreadable data, and concurrent mutation are all legitimate deltas.

use std::collections::HashMap;
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
    /// Total capacity in bytes. Container-wide on APFS.
    pub total_bytes: u64,
    /// Available capacity in bytes. Container-wide on APFS.
    pub available_bytes: u64,
    /// The `df` "Used" column in bytes. Unlike `total - available`, this one is
    /// **private to the volume** on APFS, and is the only per-volume capacity
    /// number `df` reports.
    pub used_bytes: u64,
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
        let Some(used) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
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
            used_bytes: used.saturating_mul(1_024),
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

/// Reads `<key>NAME</key><true/>` out of a `diskutil -plist` document.
///
/// A full plist parser is not a dependency this crate needs for a handful of
/// scalars; the shape is fixed and a parse miss degrades to `false`/`None`,
/// never to a wrong value.
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

/// Reads `<key>NAME</key><integer>VALUE</integer>`.
fn plist_integer(plist: &str, key: &str) -> Option<u64> {
    let needle = format!("<key>{key}</key>");
    let at = plist.find(&needle)?;
    let rest = plist.get(at + needle.len()..)?.trim_start();
    let body = rest.strip_prefix("<integer>")?;
    let end = body.find("</integer>")?;
    body.get(..end)?.trim().parse().ok()
}

/// `diskutil info -plist <target>`, or `None` if it cannot be run.
fn diskutil_info(target: &str) -> Option<String> {
    let output = Command::new("/usr/sbin/diskutil")
        .args(["info", "-plist", target])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What one `diskutil info` call tells us about a mounted volume.
///
/// One fork per volume, not four. The previous shape called `diskutil` once for
/// the name and again for removability; every field below comes out of the same
/// document.
#[derive(Debug, Default)]
struct VolumeFacts {
    name: Option<String>,
    /// `APFSContainerReference` for APFS, else `ParentWholeDisk` — the identity
    /// volumes sharing capacity have in common.
    container_id: Option<String>,
    internal: bool,
    removable: bool,
}

fn volume_facts(source: &str) -> VolumeFacts {
    let Some(plist) = diskutil_info(source) else {
        return VolumeFacts::default();
    };
    VolumeFacts {
        name: plist_string(&plist, "VolumeName").filter(|name| !name.is_empty()),
        container_id: plist_string(&plist, "APFSContainerReference")
            .or_else(|| plist_string(&plist, "ParentWholeDisk"))
            .filter(|id| !id.is_empty()),
        internal: plist_bool(&plist, "Internal"),
        removable: plist_bool(&plist, "Removable") || plist_bool(&plist, "RemovableMediaOrExternalDevice"),
    }
}

/// The physical disk behind a container, and its media name and size.
///
/// Looked up once per **container**, not once per volume: a stock Mac has one
/// container holding five or more volumes, and this is a process fork.
#[derive(Debug, Clone, Default)]
struct DiskFacts {
    /// `disk0` — the physical device, not the container.
    id: Option<String>,
    /// Media name, e.g. `APPLE SSD AP1024Z`.
    name: Option<String>,
    /// Size of the physical device, which exceeds any one container's.
    size_bytes: Option<u64>,
}

fn disk_facts(container_id: &str) -> DiskFacts {
    let Some(container) = diskutil_info(container_id) else {
        return DiskFacts::default();
    };
    // An APFS container is a *synthesized* disk: `diskutil info disk3` reports
    // `VirtualOrPhysical: Virtual`, `WholeDisk: 1`, and `ParentWholeDisk: disk3`
    // — itself. Following `ParentWholeDisk` therefore never leaves the
    // container, and a Mac with three containers on one SSD renders as three
    // separate physical disks, each captioned with the same media name. The
    // real device is behind `APFSPhysicalStores`, e.g. `disk0s2`, whose whole
    // disk is `disk0`.
    //
    // `ParentWholeDisk` remains the fallback: for a plain partition scheme
    // there is no physical store and the whole disk is already what we have.
    let id = plist_string(&container, "APFSPhysicalStore")
        .as_deref()
        .and_then(whole_disk_of)
        .or_else(|| plist_string(&container, "ParentWholeDisk"))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| container_id.to_owned());
    let Some(disk) = diskutil_info(&id) else {
        return DiskFacts {
            id: Some(id),
            ..DiskFacts::default()
        };
    };
    DiskFacts {
        name: plist_string(&disk, "MediaName").filter(|name| !name.is_empty()),
        size_bytes: plist_integer(&disk, "Size").or_else(|| plist_integer(&disk, "TotalSize")),
        id: Some(id),
    }
}

/// The whole disk a slice belongs to: `disk0s2` -> `disk0`.
///
/// Returns `None` for anything that is not `disk<digits>[s<digits>…]`, so a
/// device naming scheme this does not recognise degrades to the
/// `ParentWholeDisk` fallback rather than to a wrong grouping.
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

/// Whether macOS owns this mount rather than the user.
///
/// The sealed system volume mounts at `/`, and the auxiliary APFS volumes of a
/// macOS install (Preboot, Recovery, VM, Update, xART, iSCPreboot, Hardware)
/// mount under `/System/Volumes/`. `/System/Volumes/Data` is the exception: it
/// *is* the user's data volume, and it is where a "scan my files" run belongs.
fn is_system_volume(mount_point: &Path, is_root: bool) -> bool {
    if mount_point == Path::new("/System/Volumes/Data") {
        return false;
    }
    is_root || mount_point.starts_with("/System/Volumes/")
}

/// Lists mounted local volumes for the launch screen.
///
/// Blocking: it runs `df`, and `diskutil`/`tmutil` once per volume. Always call
/// it from `spawn_blocking`.
#[must_use]
pub(crate) fn list() -> Vec<VolumeInfo> {
    let root_device = std::fs::metadata("/").map(|metadata| metadata.dev()).ok();
    // One `diskutil info` per *container*, not per volume: a stock Mac has five
    // or more volumes sharing one, and each lookup is two process forks.
    let mut disks: HashMap<String, DiskFacts> = HashMap::new();

    read_mounts()
        .into_iter()
        .filter_map(|mount| {
            let device = std::fs::metadata(&mount.mount_point)
                .map(|metadata| metadata.dev())
                .ok()?;
            let facts = volume_facts(&mount.source);
            let disk = facts.container_id.as_ref().map_or_else(DiskFacts::default, |id| {
                disks.entry(id.clone()).or_insert_with(|| disk_facts(id)).clone()
            });
            let is_root_volume = root_device == Some(device);

            Some(VolumeInfo {
                name: facts.name.clone().unwrap_or_else(|| {
                    mount.mount_point.file_name().map_or_else(
                        || mount.mount_point.to_string_lossy().into_owned(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                }),
                mount_point: DisplayPath::from_bytes(mount.mount_point.as_os_str().as_encoded_bytes()),
                device,
                fs_type: mount.fs_type.clone(),
                total_bytes: mount.total_bytes,
                available_bytes: mount.available_bytes,
                // The one capacity number that is this volume's own. On APFS
                // `total - available` is the *container's* usage, which is why
                // five volumes of one Mac each appeared to have used 968 GB.
                used_bytes: mount.used_bytes,
                device_node: mount.source.clone(),
                container_id: facts.container_id,
                disk_id: disk.id,
                disk_name: disk.name,
                disk_size_bytes: disk.size_bytes,
                is_internal: facts.internal,
                is_system: is_system_volume(&mount.mount_point, is_root_volume),
                // `volumeAvailableCapacityForImportantUsage` is a Foundation
                // resource value and needs an ObjC bridge; reporting `None` is
                // honest, reporting `available_bytes` twice would not be.
                important_available_bytes: None,
                is_root_volume,
                is_removable: facts.removable,
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
    fn used_bytes_is_read_from_df_rather_than_derived_from_total_minus_available() {
        // The bug this guards: every APFS volume in one container reports the
        // *container's* total and available, so `total - available` is the
        // same (huge) number for all of them. `Used` is per volume.
        let entries = parse_df(SAMPLE);
        let root = &entries[0];
        let data = &entries[2];

        assert_eq!(root.total_bytes, data.total_bytes, "one container, one capacity");
        assert_eq!(root.available_bytes, data.available_bytes);

        assert_eq!(root.used_bytes, 10_222_448 * 1_024);
        assert_eq!(data.used_bytes, 800_000_000 * 1_024);
        assert_ne!(root.used_bytes, data.used_bytes, "but usage is per volume");

        let derived = root.total_bytes - root.available_bytes;
        assert!(
            derived > root.used_bytes * 10,
            "total-minus-available over-reports a sealed system volume by orders of magnitude"
        );
    }

    #[test]
    fn a_slice_resolves_to_its_whole_disk() {
        // The case that matters: an APFS container's physical store is a
        // partition, and the group the UI draws is the *device* behind it.
        assert_eq!(whole_disk_of("disk0s2").as_deref(), Some("disk0"));
        assert_eq!(whole_disk_of("disk10s1").as_deref(), Some("disk10"));
        assert_eq!(whole_disk_of("disk0").as_deref(), Some("disk0"));
        // Unrecognised shapes degrade to None, so the caller falls back to
        // ParentWholeDisk instead of inventing a grouping.
        assert_eq!(whole_disk_of("/dev/disk0s2"), None);
        assert_eq!(whole_disk_of("diskAs2"), None);
        assert_eq!(whole_disk_of("md0"), None);
    }

    #[test]
    fn every_container_on_one_device_reports_the_same_physical_disk() {
        // A stock Apple silicon Mac has three synthesized containers — disk1
        // (iSC), disk2 (recovery), disk3 (the install) — all on disk0. Before
        // the physical-store lookup these each reported themselves as the
        // whole disk, and the picker drew three "APPLE SSD" groups for one SSD.
        let Some(root_container) = volume_facts("/dev/disk3s1s1").container_id else {
            return; // diskutil unavailable in this environment.
        };
        let facts = disk_facts(&root_container);
        let Some(id) = facts.id else { return };
        assert_eq!(
            whole_disk_of(&id).as_deref(),
            Some(id.as_str()),
            "a physical disk id is already whole; there is no slice suffix left to strip"
        );
        assert_ne!(
            id, root_container,
            "the physical disk is not the synthesized container it backs"
        );
        if let Some(size) = facts.size_bytes {
            assert!(
                size >= 100_000_000_000,
                "the physical device is at least as large as its containers, got {size}"
            );
        }
    }

    #[test]
    fn the_data_volume_is_not_a_system_volume_but_its_siblings_are() {
        // `/System/Volumes/Data` is where the user's files live; the rest of
        // `/System/Volumes/*` is the OS install.
        assert!(!is_system_volume(Path::new("/System/Volumes/Data"), false));
        assert!(is_system_volume(Path::new("/System/Volumes/Preboot"), false));
        assert!(is_system_volume(Path::new("/System/Volumes/VM"), false));
        assert!(is_system_volume(Path::new("/"), true), "the sealed root is the OS");
        assert!(!is_system_volume(Path::new("/Volumes/Time Machine"), false));
    }

    #[test]
    fn plist_scalars_read_or_degrade_to_none() {
        let plist = "<dict><key>Removable</key><true/><key>VolumeName</key><string>Macintosh HD</string><key>Size</key><integer>994662584320</integer></dict>";
        assert!(plist_bool(plist, "Removable"));
        assert!(!plist_bool(plist, "Ejectable"));
        assert_eq!(plist_string(plist, "VolumeName").as_deref(), Some("Macintosh HD"));
        assert_eq!(plist_string(plist, "Missing"), None);
        assert_eq!(plist_integer(plist, "Size"), Some(994_662_584_320));
        assert_eq!(plist_integer(plist, "VolumeName"), None, "a string is not an integer");
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
    fn the_running_machine_groups_its_apfs_volumes_into_containers() {
        // Not an assertion about *this* Mac's topology: only that when the
        // container is readable at all, the volumes sharing one capacity agree
        // on which container they share, which is what the UI groups by.
        let volumes = list();
        let root = volumes
            .iter()
            .find(|volume| volume.is_root_volume)
            .expect("a boot volume");
        if root.fs_type != "apfs" {
            return;
        }
        let Some(container) = root.container_id.as_deref() else {
            return; // diskutil unavailable; degrading to no grouping is allowed.
        };
        for volume in volumes
            .iter()
            .filter(|other| other.container_id.as_deref() == Some(container))
        {
            assert_eq!(
                volume.total_bytes, root.total_bytes,
                "volumes in one container report one capacity"
            );
            assert!(
                volume.used_bytes <= volume.total_bytes,
                "a volume cannot use more than its container holds"
            );
        }
    }
}
