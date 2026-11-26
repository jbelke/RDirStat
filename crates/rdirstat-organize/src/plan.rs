//! The plan file and the token that binds it.
//!
//! `rdirstat backup plan` writes a [`PlanFile`] as JSON; `rdirstat backup
//! apply --plan <file> --token <hex>` refuses the whole apply unless
//! [`plan_token`] of the file it read equals the token the user passed.
//!
//! # Paths
//!
//! Every path in a plan is hashed from its raw `OsStr` bytes, so the token
//! is stable for any name. The JSON form, however, cannot carry a non-UTF-8
//! `PathBuf` (serde refuses it), so [`crate::fit`] keeps such items out of
//! `items` and lists them as skipped under
//! [`crate::fit::RULE_NON_UTF8`]. A `source_root` or `dest_root` that is
//! not UTF-8 makes serialization fail with an error rather than writing a
//! lossy path that apply would then copy to the wrong place.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fit::{ItemKind, PlanItem, SkippedItem};
use crate::rules::hex;

/// Version of the [`PlanFile`] layout and of the token encoding.
pub const PLAN_SCHEMA_VERSION: u32 = 2;
/// The only intent the CLI may apply.
pub const INTENT_BACKUP: &str = "backup";

/// A Backup plan as written to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFile {
    /// [`PLAN_SCHEMA_VERSION`] at the time of writing.
    pub schema_version: u32,
    /// [`INTENT_BACKUP`].
    pub intent: String,
    /// Source volume root the relative paths hang from.
    pub source_root: PathBuf,
    /// Label under `Backups/` on the destination.
    pub source_label: String,
    /// Destination volume root.
    pub dest_root: PathBuf,
    /// Physical disk of the destination as a BSD name (`disk14`). Shown to
    /// the user and used for the same-disk pair check; not an identity,
    /// because macOS renumbers disks on every plug and boot.
    pub dest_disk_id: String,
    /// Volume UUID of the destination. Apply refuses unless the volume
    /// mounted at `dest_root` has this UUID.
    pub dest_volume_uuid: String,
    /// Identity of the scan the plan was made from.
    pub source_snapshot: String,
    /// [`crate::Rules::hash_hex`] of the rules used.
    pub rules_hash: String,
    /// [`crate::Rules::version`] of the rules used.
    pub rules_version: u32,
    /// Floor on allocated bytes the fit used.
    pub floor_allocated: u64,
    /// Items to copy, in plan order.
    pub items: Vec<PlanItem>,
    /// How many candidates the rules skipped.
    pub skipped_count: u64,
    /// Every skipped candidate over the floor, largest first, each with the
    /// rule id and reason that left it out. Not in the token: it follows
    /// from the snapshot and the rules hash, which are.
    #[serde(default)]
    pub skipped: Vec<SkippedItem>,
    /// When the plan was made, milliseconds since the epoch. Not in the token.
    pub created_at_ms: i64,
}

/// Lowercase hex SHA-256 binding a plan.
///
/// The digest covers every field of the plan except `created_at_ms`:
/// schema version, intent, source root, source label, destination root,
/// destination `disk_id` and volume UUID, source snapshot, rules hash and version, floor,
/// skipped count, and every item (relative and absolute path bytes, kind,
/// logical, allocated, dev, ino, mtime) in order. Each variable-length
/// field is length-prefixed so no two different plans encode alike.
/// Changing any of them changes the token; re-saving the same plan at
/// another time does not.
#[must_use]
pub fn plan_token(plan: &PlanFile) -> String {
    let mut h = Sha256::new();
    h.update(b"rdirstat-plan-token\0");
    h.update(plan.schema_version.to_le_bytes());
    bytes(&mut h, plan.intent.as_bytes());
    path(&mut h, &plan.source_root);
    bytes(&mut h, plan.source_label.as_bytes());
    path(&mut h, &plan.dest_root);
    bytes(&mut h, plan.dest_disk_id.as_bytes());
    bytes(&mut h, plan.dest_volume_uuid.as_bytes());
    bytes(&mut h, plan.source_snapshot.as_bytes());
    bytes(&mut h, plan.rules_hash.as_bytes());
    h.update(plan.rules_version.to_le_bytes());
    h.update(plan.floor_allocated.to_le_bytes());
    h.update(plan.skipped_count.to_le_bytes());
    h.update((plan.items.len() as u64).to_le_bytes());
    for item in &plan.items {
        path(&mut h, &item.relative);
        path(&mut h, &item.absolute);
        h.update([kind_tag(item.kind)]);
        h.update(item.logical.to_le_bytes());
        h.update(item.allocated.to_le_bytes());
        h.update(item.dev.to_le_bytes());
        h.update(item.ino.to_le_bytes());
        h.update(item.mtime_ns.to_le_bytes());
    }
    hex(&h.finalize())
}

fn bytes(h: &mut Sha256, data: &[u8]) {
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}

fn path(h: &mut Sha256, p: &Path) {
    use std::os::unix::ffi::OsStrExt;
    bytes(h, p.as_os_str().as_bytes());
}

fn kind_tag(kind: ItemKind) -> u8 {
    match kind {
        ItemKind::File => 1,
        ItemKind::Package => 2,
        ItemKind::Dir => 3,
        ItemKind::Symlink => 4,
        ItemKind::Special => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fit::{DEFAULT_CAP, DEFAULT_RESERVE_PRESSURE, FitInput, ScanItem, fit};
    use crate::pair::DiskFacts;
    use crate::rules::Rules;

    fn sample() -> PlanFile {
        PlanFile {
            schema_version: PLAN_SCHEMA_VERSION,
            intent: INTENT_BACKUP.into(),
            source_root: "/System/Volumes/Data".into(),
            source_label: "mac-data".into(),
            dest_root: "/Volumes/gdr22tb".into(),
            dest_disk_id: "disk14".into(),
            dest_volume_uuid: "B6C3D4E7-1AA7-3BD1-B725-6033F3101516".into(),
            source_snapshot: "scan-abc".into(),
            rules_hash: Rules::default_backup().hash_hex(),
            rules_version: 1,
            floor_allocated: 100 << 20,
            items: vec![
                PlanItem {
                    relative: "Users/josh/Movies/a.mov".into(),
                    absolute: "/System/Volumes/Data/Users/josh/Movies/a.mov".into(),
                    kind: ItemKind::File,
                    logical: 5 << 30,
                    allocated: 5 << 30,
                    dev: 16_777_234,
                    ino: 42,
                    mtime_ns: -1_i128 << 70,
                },
                PlanItem {
                    relative: "Users/josh/Pictures/P.photoslibrary".into(),
                    absolute: "/System/Volumes/Data/Users/josh/Pictures/P.photoslibrary".into(),
                    kind: ItemKind::Package,
                    logical: 1 << 30,
                    allocated: 1 << 30,
                    dev: 16_777_234,
                    ino: 43,
                    mtime_ns: 1_700_000_000_000_000_000,
                },
            ],
            skipped_count: 1,
            skipped: vec![SkippedItem {
                relative: "Users/josh/src/web/node_modules".into(),
                rule_id: "node-modules".into(),
                reason: "node_modules".into(),
                allocated: 200 << 20,
            }],
            created_at_ms: 1_764_000_000_000,
        }
    }

    #[test]
    fn token_is_stable_hex_and_ignores_created_at() {
        let a = sample();
        let t = plan_token(&a);
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(plan_token(&a), t);
        let mut later = a.clone();
        later.created_at_ms += 10_000;
        assert_eq!(plan_token(&later), t);
        later.skipped.clear();
        assert_eq!(plan_token(&later), t, "the skip log is derived, not bound");
    }

    #[test]
    fn token_changes_with_any_bound_field() {
        type Mutation = Box<dyn Fn(&mut PlanFile)>;
        let base = plan_token(&sample());
        let mutations: Vec<(&str, Mutation)> = vec![
            ("item size", Box::new(|p| p.items[0].logical += 1)),
            ("item mtime", Box::new(|p| p.items[0].mtime_ns += 1)),
            ("item ino", Box::new(|p| p.items[1].ino += 1)),
            ("item kind", Box::new(|p| p.items[1].kind = ItemKind::File)),
            ("item path", Box::new(|p| p.items[0].relative.push("x"))),
            ("item removed", Box::new(|p| drop(p.items.pop()))),
            ("item order", Box::new(|p| p.items.swap(0, 1))),
            ("rules hash", Box::new(|p| p.rules_hash = "0".repeat(64))),
            ("rules version", Box::new(|p| p.rules_version = 2)),
            ("dest disk", Box::new(|p| p.dest_disk_id = "disk15".into())),
            ("dest volume", Box::new(|p| p.dest_volume_uuid = "0".into())),
            ("snapshot", Box::new(|p| p.source_snapshot = "scan-abd".into())),
            ("source root", Box::new(|p| p.source_root = "/Volumes/tuf8tb".into())),
            ("label", Box::new(|p| p.source_label = "tuf8tb".into())),
            ("dest root", Box::new(|p| p.dest_root = "/Volumes/other".into())),
            ("intent", Box::new(|p| p.intent = "migrate".into())),
            ("floor", Box::new(|p| p.floor_allocated = 1)),
        ];
        for (what, mutate) in mutations {
            let mut p = sample();
            mutate(&mut p);
            assert_ne!(plan_token(&p), base, "{what}");
        }
    }

    #[test]
    fn length_prefixes_prevent_field_shifting() {
        let mut a = sample();
        a.source_label = "ab".into();
        a.dest_disk_id = "c".into();
        let mut b = sample();
        b.source_label = "a".into();
        b.dest_disk_id = "bc".into();
        assert_ne!(plan_token(&a), plan_token(&b));
    }

    #[test]
    fn token_changes_when_the_rules_file_changes() {
        let mut a = sample();
        a.rules_hash = Rules::from_toml("version = 1\n")
            .unwrap_or_else(|e| unreachable!("{e}"))
            .hash_hex();
        assert_ne!(plan_token(&a), plan_token(&sample()));
    }

    #[test]
    fn json_round_trip_keeps_the_token() {
        let p = sample();
        let json = serde_json::to_string(&p).unwrap_or_else(|e| unreachable!("{e}"));
        assert!(json.contains("\"kind\":\"package\""));
        assert!(json.contains("\"rule_id\":\"node-modules\""));
        let back: PlanFile = serde_json::from_str(&json).unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(back, p);
        assert_eq!(plan_token(&back), plan_token(&p));
    }

    #[test]
    fn non_utf8_root_refuses_to_serialize() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut p = sample();
        p.source_root = PathBuf::from(OsStr::from_bytes(b"/Volumes/\xff"));
        assert!(serde_json::to_string(&p).is_err());
        // Hashing still works on the bytes.
        assert_eq!(plan_token(&p).len(), 64);
    }

    #[test]
    fn a_fit_plan_becomes_a_tokened_plan_file() {
        let rules = Rules::default_backup();
        let d = DiskFacts {
            disk_id: Some("disk14".into()),
            volume_uuid: None,
            st_dev: 2,
            filesystem: "apfs".into(),
            total_bytes: 1 << 40,
            available_bytes: 1 << 40,
        };
        let items = [ScanItem {
            relative: "Users/a/Movies/a.mov".into(),
            absolute: "/System/Volumes/Data/Users/a/Movies/a.mov".into(),
            kind: ItemKind::File,
            logical: 1 << 30,
            allocated: 1 << 30,
            dev: 1,
            ino: 5,
            mtime_ns: 7,
            tie: 0,
        }];
        let fp = fit(&FitInput {
            items: &items,
            rules: &rules,
            floor_allocated: 0,
            dest: &d,
            reserve_pressure: DEFAULT_RESERVE_PRESSURE,
            cap: DEFAULT_CAP,
        });
        let mut p = sample();
        p.items = fp.items;
        p.skipped_count = fp.skipped_count;
        p.skipped = fp.skipped;
        let t1 = plan_token(&p);
        let json = serde_json::to_vec(&p).unwrap_or_else(|e| unreachable!("{e}"));
        let back: PlanFile = serde_json::from_slice(&json).unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(plan_token(&back), t1);
    }
}
