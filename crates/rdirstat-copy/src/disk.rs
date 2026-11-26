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

use plist::{Dictionary, Value};

use crate::{CopyError, c_path, run};

/// The facts a backup pair is judged on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskIdentity {
    /// Physical whole disk, e.g. `disk14`. `None` when `diskutil` cannot say;
    /// callers treat unknown as "refuse", never as "different".
    ///
    /// For a volume inside a mounted disk image this is the physical disk
    /// holding the image *file*, never the image's own virtual device.
    pub disk_id: Option<String>,
    /// APFS `VolumeUUID` (or the filesystem's volume UUID) of the volume at
    /// `mount_point`. Unlike `disk_id`, which macOS renumbers on every plug
    /// and boot, this names the volume itself. `None` when `diskutil` cannot
    /// say.
    pub volume_uuid: Option<String>,
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
/// an error: it degrades `disk_id` and `volume_uuid` to `None`.
pub fn disk_identity(path: &Path) -> Result<DiskIdentity, CopyError> {
    let st_dev = std::fs::metadata(path)
        .map_err(|error| CopyError::io(path, error))?
        .dev();
    let c = c_path(path)?;
    let fs = statfs_of(&c).map_err(|error| CopyError::io(path, error))?;
    let tools = SystemTools;
    let (disk_id, volume_uuid) = if fs.mounted_from.starts_with("/dev/") {
        (
            physical_disk_of(&fs.mounted_from, &tools, 0),
            volume_uuid_of(&fs.mounted_from, &tools),
        )
    } else {
        (None, None)
    };
    Ok(DiskIdentity {
        disk_id,
        volume_uuid,
        filesystem: fs.filesystem,
        st_dev,
        case_sensitive: case_sensitive(&c),
        total_bytes: fs.total_bytes,
        available_bytes: fs.available_bytes,
        mount_point: fs.mount_point,
    })
}

/// What resolving a disk asks the OS. Tests substitute canned plists.
trait DiskTools {
    /// `diskutil info -plist <target>`, parsed.
    fn diskutil_info(&self, target: &str) -> Option<Dictionary>;
    /// `hdiutil info -plist`, parsed.
    fn hdiutil_info(&self) -> Option<Dictionary>;
    /// `f_mntfromname` of the filesystem holding `path`.
    fn device_of(&self, path: &Path) -> Option<String>;
}

struct SystemTools;

impl DiskTools for SystemTools {
    fn diskutil_info(&self, target: &str) -> Option<Dictionary> {
        let stdout = run(
            "diskutil",
            Command::new("/usr/sbin/diskutil").args(["info", "-plist", target]),
        )
        .ok()?;
        parse_dict(&stdout)
    }

    fn hdiutil_info(&self) -> Option<Dictionary> {
        let stdout = run("hdiutil", Command::new("/usr/bin/hdiutil").args(["info", "-plist"])).ok()?;
        parse_dict(&stdout)
    }

    fn device_of(&self, path: &Path) -> Option<String> {
        statfs_of(&c_path(path).ok()?).ok().map(|fs| fs.mounted_from)
    }
}

fn parse_dict(bytes: &[u8]) -> Option<Dictionary> {
    Value::from_reader(io::Cursor::new(bytes)).ok()?.into_dictionary()
}

/// How many disk images inside disk images are followed before giving up.
const MAX_IMAGE_DEPTH: usize = 4;

/// `/dev/disk3s5` → `disk0`: the whole disk under the volume's single APFS
/// physical store, or the partition's whole disk for a plain scheme. When
/// that whole disk is a mounted disk image, the answer is the physical disk
/// holding the image file.
///
/// `None` — which callers treat as "refuse" — whenever any step cannot be
/// answered: `diskutil` fails, an APFS container lists no physical store or
/// more than one (Fusion), the whole disk is virtual but no image backs it,
/// or the image's backing file cannot be found. It never falls back to a
/// container or virtual device id, because two virtual ids on one physical
/// disk would look "different".
fn physical_disk_of(device_node: &str, tools: &dyn DiskTools, depth: usize) -> Option<String> {
    if depth > MAX_IMAGE_DEPTH {
        return None;
    }
    let volume = tools.diskutil_info(device_node)?;
    let whole = match dict_string(&volume, "APFSContainerReference").filter(|id| !id.is_empty()) {
        Some(container) => {
            let info = tools.diskutil_info(container)?;
            let stores = physical_stores(&info);
            let [store] = stores.as_slice() else {
                return None;
            };
            whole_disk_of(store)?
        }
        None => whole_disk_of(dict_string(&volume, "ParentWholeDisk")?)?,
    };
    let info = tools.diskutil_info(&whole)?;
    let is_image = dict_string(&info, "BusProtocol") == Some("Disk Image");
    let is_virtual = dict_string(&info, "VirtualOrPhysical") == Some("Virtual");
    if !is_image && !is_virtual {
        return Some(whole);
    }
    let image = image_path_of(&tools.hdiutil_info()?, &whole)?;
    let backing = tools.device_of(Path::new(&image))?;
    if !backing.starts_with("/dev/") {
        return None;
    }
    physical_disk_of(&backing, tools, depth + 1)
}

/// The volume UUID `diskutil` reports for a device node.
fn volume_uuid_of(device_node: &str, tools: &dyn DiskTools) -> Option<String> {
    let volume = tools.diskutil_info(device_node)?;
    dict_string(&volume, "VolumeUUID")
        .map(str::trim)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_owned)
}

/// Every distinct `APFSPhysicalStore` slice a container lists, from the
/// `APFSPhysicalStores` array or, on older systems, the single key.
fn physical_stores(container: &Dictionary) -> Vec<String> {
    let mut stores: Vec<String> = Vec::new();
    if let Some(array) = container.get("APFSPhysicalStores").and_then(Value::as_array) {
        for store in array {
            if let Some(id) = store
                .as_dictionary()
                .and_then(|entry| dict_string(entry, "APFSPhysicalStore"))
            {
                stores.push(id.to_owned());
            }
        }
    }
    if let Some(single) = dict_string(container, "APFSPhysicalStore") {
        stores.push(single.to_owned());
    }
    stores.retain(|id| !id.is_empty());
    stores.sort();
    stores.dedup();
    stores
}

/// The `image-path` of the attached image one of whose system entities is
/// `/dev/<whole>`.
fn image_path_of(info: &Dictionary, whole: &str) -> Option<String> {
    let dev_entry = format!("/dev/{whole}");
    info.get("images")?.as_array()?.iter().find_map(|image| {
        let image = image.as_dictionary()?;
        let backs = image
            .get("system-entities")?
            .as_array()?
            .iter()
            .filter_map(Value::as_dictionary)
            .any(|entity| dict_string(entity, "dev-entry") == Some(dev_entry.as_str()));
        backs
            .then(|| dict_string(image, "image-path").map(str::to_owned))
            .flatten()
    })
}

fn dict_string<'a>(dict: &'a Dictionary, key: &str) -> Option<&'a str> {
    dict.get(key)?.as_string()
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

    use std::collections::HashMap;

    /// Canned `diskutil` / `hdiutil` / `statfs` answers.
    #[derive(Default)]
    struct Fake {
        diskutil: HashMap<String, String>,
        hdiutil: Option<String>,
        devices: HashMap<PathBuf, String>,
    }

    impl Fake {
        fn disk(mut self, target: &str, body: &str) -> Self {
            self.diskutil.insert(target.to_owned(), plist_xml(body));
            self
        }
    }

    impl DiskTools for Fake {
        fn diskutil_info(&self, target: &str) -> Option<Dictionary> {
            parse_dict(self.diskutil.get(target)?.as_bytes())
        }
        fn hdiutil_info(&self) -> Option<Dictionary> {
            parse_dict(self.hdiutil.as_ref()?.as_bytes())
        }
        fn device_of(&self, path: &Path) -> Option<String> {
            self.devices.get(path).cloned()
        }
    }

    fn plist_xml(body: &str) -> String {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>{body}</dict></plist>")
    }

    fn s(key: &str, value: &str) -> String {
        format!("<key>{key}</key><string>{value}</string>")
    }

    fn stores(slices: &[&str]) -> String {
        let entries = slices.iter().fold(String::new(), |mut out, slice| {
            out.push_str("<dict>");
            out.push_str(&s("APFSPhysicalStore", slice));
            out.push_str("</dict>");
            out
        });
        format!("<key>APFSPhysicalStores</key><array>{entries}</array>")
    }

    /// The boot SSD as this Mac reports it: volume disk3s5 in synthesized
    /// container disk3 (virtual, Apple Fabric) on physical store disk0s2.
    fn internal_ssd() -> Fake {
        Fake::default()
            .disk(
                "/dev/disk3s5",
                &[
                    s("APFSContainerReference", "disk3"),
                    s("ParentWholeDisk", "disk3"),
                    s("VolumeUUID", "AAAA-DATA"),
                ]
                .concat(),
            )
            .disk(
                "disk3",
                &[
                    stores(&["disk0s2"]),
                    s("BusProtocol", "Apple Fabric"),
                    s("ParentWholeDisk", "disk3"),
                    s("VirtualOrPhysical", "Virtual"),
                ]
                .concat(),
            )
            .disk(
                "disk0",
                &[s("BusProtocol", "Apple Fabric"), s("VirtualOrPhysical", "Unknown")].concat(),
            )
    }

    #[test]
    fn an_apfs_volume_resolves_to_its_physical_store() {
        let fake = internal_ssd();
        assert_eq!(physical_disk_of("/dev/disk3s5", &fake, 0).as_deref(), Some("disk0"));
        assert_eq!(volume_uuid_of("/dev/disk3s5", &fake).as_deref(), Some("AAAA-DATA"));
    }

    #[test]
    fn a_plain_partition_resolves_to_its_whole_disk() {
        let fake = Fake::default()
            .disk("/dev/disk14s1", &s("ParentWholeDisk", "disk14"))
            .disk(
                "disk14",
                &[s("BusProtocol", "USB"), s("VirtualOrPhysical", "Physical")].concat(),
            );
        assert_eq!(physical_disk_of("/dev/disk14s1", &fake, 0).as_deref(), Some("disk14"));
    }

    /// `~/Backup.sparsebundle` on the boot SSD, mounted at /Volumes/Backup:
    /// volume disk9s1 in container disk9 on store disk8s1, whole disk disk8
    /// is the image. The answer must be the SSD (disk0), so the pair check
    /// sees one physical disk and refuses.
    #[test]
    fn a_disk_image_resolves_to_the_disk_holding_its_file() {
        let image = PathBuf::from("/System/Volumes/Data/Users/u/Backup.sparsebundle");
        let mut fake = internal_ssd()
            .disk("/dev/disk9s1", &s("APFSContainerReference", "disk9"))
            .disk(
                "disk9",
                &[stores(&["disk8s1"]), s("BusProtocol", "Disk Image")].concat(),
            )
            .disk(
                "disk8",
                &[s("BusProtocol", "Disk Image"), s("VirtualOrPhysical", "Virtual")].concat(),
            );
        fake.hdiutil = Some(plist_xml(&format!(
            "<key>images</key><array><dict>{}<key>system-entities</key><array>\
             <dict>{}</dict><dict>{}</dict></array></dict></array>",
            s("image-path", &image.to_string_lossy()),
            s("dev-entry", "/dev/disk8"),
            s("dev-entry", "/dev/disk8s1"),
        )));
        fake.devices.insert(image, "/dev/disk3s5".into());
        assert_eq!(physical_disk_of("/dev/disk9s1", &fake, 0).as_deref(), Some("disk0"));
    }

    #[test]
    fn a_disk_image_whose_file_cannot_be_found_is_unknown() {
        let fake = Fake::default()
            .disk("/dev/disk9s1", &s("ParentWholeDisk", "disk9"))
            .disk("disk9", &s("BusProtocol", "Disk Image"));
        assert_eq!(physical_disk_of("/dev/disk9s1", &fake, 0), None, "no hdiutil answer");

        let mut listed_elsewhere = Fake::default()
            .disk("/dev/disk9s1", &s("ParentWholeDisk", "disk9"))
            .disk("disk9", &s("VirtualOrPhysical", "Virtual"));
        listed_elsewhere.hdiutil = Some(plist_xml("<key>images</key><array></array>"));
        assert_eq!(
            physical_disk_of("/dev/disk9s1", &listed_elsewhere, 0),
            None,
            "not attached"
        );
    }

    #[test]
    fn a_failed_container_lookup_is_unknown_never_the_container() {
        let mut fake = internal_ssd();
        fake.diskutil.remove("disk3");
        assert_eq!(physical_disk_of("/dev/disk3s5", &fake, 0), None);
    }

    #[test]
    fn a_container_without_a_physical_store_is_unknown() {
        let fake = internal_ssd().disk(
            "disk3",
            &[s("ParentWholeDisk", "disk3"), s("VirtualOrPhysical", "Virtual")].concat(),
        );
        assert_eq!(physical_disk_of("/dev/disk3s5", &fake, 0), None);
    }

    #[test]
    fn a_fusion_container_with_two_stores_is_unknown() {
        let fake = internal_ssd().disk("disk3", &stores(&["disk0s2", "disk1s2"]));
        assert_eq!(physical_disk_of("/dev/disk3s5", &fake, 0), None);
    }

    #[test]
    fn a_failed_whole_disk_lookup_is_unknown() {
        let mut fake = internal_ssd();
        fake.diskutil.remove("disk0");
        assert_eq!(physical_disk_of("/dev/disk3s5", &fake, 0), None);
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
