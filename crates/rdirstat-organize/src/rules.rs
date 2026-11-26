//! Skip and include rules, read from a versioned TOML file.
//!
//! The file format is documented at the top of `rules/backup.toml`, the
//! default. In short:
//!
//! ```toml
//! version = 1
//!
//! [[rule]]
//! id = "node-modules"
//! reason = "node_modules: restored from the lockfile"
//! action = "skip"            # or "include"
//! globs = ["**/node_modules"]
//! ```
//!
//! # Matching
//!
//! Paths are relative to the source volume root. A rule *applies* to a path
//! when one of its globs matches the path itself or any ancestor directory
//! of it, so a rule naming a directory covers its whole subtree. Rules are
//! tried in file order and the first rule that applies wins, whichever
//! ancestor it matched: an `include` rule written above a `skip` rule rescues
//! the paths it names, and one written below it never sees them. `*` and `?`
//! do not cross `/`. Matching is on the stored name bytes and is
//! case-sensitive, so non-UTF-8 names match too.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What a matching rule does to the paths it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    /// Leave the path, and everything under it, out of the plan.
    Skip,
    /// Keep the path in the plan even if a later rule would skip it.
    Include,
}

/// One rule as written in the rules file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Stable identifier, reported beside every item the rule skipped.
    pub id: String,
    /// Human sentence explaining why the rule exists.
    pub reason: String,
    /// Skip or include.
    pub action: RuleAction,
    /// Globs relative to the source volume root.
    pub globs: Vec<String>,
}

/// Why a rules file was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RulesError {
    /// The text is not valid TOML or does not have the rules shape.
    #[error("rules file is not valid: {0}")]
    Parse(#[from] toml::de::Error),
    /// `version` is missing or zero.
    #[error("rules file version must be 1 or greater")]
    Version,
    /// Two rules share an id, so a skipped item could not name its rule.
    #[error("rule id {0:?} appears more than once")]
    DuplicateId(String),
    /// A rule is missing its id, its reason, or its globs.
    #[error("rule {id:?} is incomplete: {what}")]
    Incomplete {
        /// The offending rule's id (may be empty).
        id: String,
        /// Which part is missing.
        what: &'static str,
    },
    /// A glob does not compile.
    #[error("rule {id:?} has an invalid glob {glob:?}: {source}")]
    Glob {
        /// The rule's id.
        id: String,
        /// The glob as written.
        glob: String,
        /// The compiler's complaint.
        source: globset::Error,
    },
    /// The compiled set could not be built.
    #[error("rules could not be compiled: {0}")]
    Build(globset::Error),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    version: u32,
    #[serde(default)]
    rule: Vec<Rule>,
}

/// A compiled, versioned rule list.
#[derive(Debug, Clone)]
pub struct Rules {
    version: u32,
    list: Vec<Rule>,
    set: GlobSet,
    /// `owner[i]` is the index in `list` of glob `i` in `set`.
    owner: Vec<usize>,
    hash: String,
}

/// The default rules file, compiled into the crate.
const DEFAULT_BACKUP: &str = include_str!("../rules/backup.toml");

impl Rules {
    /// The default Backup rules, `rules/backup.toml`.
    ///
    /// # Panics
    ///
    /// Never in practice: the file is compiled in, and the
    /// `default_rules_parse` test fails the build if it stops parsing.
    #[must_use]
    #[allow(
        clippy::expect_used,
        reason = "the file is compiled in and `default_rules_parse` proves it parses"
    )]
    pub fn default_backup() -> Rules {
        Rules::from_toml(DEFAULT_BACKUP).expect("rules/backup.toml is valid")
    }

    /// Parse and compile a rules file.
    ///
    /// # Errors
    ///
    /// Refuses invalid TOML, unknown keys, a zero version, duplicate or empty
    /// ids, a rule without a reason or globs, and globs that do not compile.
    pub fn from_toml(text: &str) -> Result<Rules, RulesError> {
        let file: RulesFile = toml::from_str(text)?;
        if file.version == 0 {
            return Err(RulesError::Version);
        }
        let mut builder = GlobSetBuilder::new();
        let mut owner = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (index, rule) in file.rule.iter().enumerate() {
            let incomplete = |what| RulesError::Incomplete {
                id: rule.id.clone(),
                what,
            };
            if rule.id.trim().is_empty() {
                return Err(incomplete("id is empty"));
            }
            if rule.reason.trim().is_empty() {
                return Err(incomplete("reason is empty"));
            }
            if rule.globs.is_empty() {
                return Err(incomplete("globs is empty"));
            }
            if !seen.insert(rule.id.as_str()) {
                return Err(RulesError::DuplicateId(rule.id.clone()));
            }
            for glob in &rule.globs {
                let compiled = GlobBuilder::new(glob)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .map_err(|source| RulesError::Glob {
                        id: rule.id.clone(),
                        glob: glob.clone(),
                        source,
                    })?;
                builder.add(compiled);
                owner.push(index);
            }
        }
        let set = builder.build().map_err(RulesError::Build)?;
        Ok(Rules {
            version: file.version,
            list: file.rule,
            set,
            owner,
            hash: hex(&Sha256::digest(text.as_bytes())),
        })
    }

    /// The first rule, in file order, that applies to `relative_path` or to
    /// any of its ancestor directories. `None` when no rule applies.
    ///
    /// First match wins, so an `include` rule above a `skip` rule beats it.
    /// A leading `/` or `./` is ignored; the path is always read as relative
    /// to the source volume root.
    #[must_use]
    pub fn matched(&self, relative_path: &Path) -> Option<&Rule> {
        let path = normalize(relative_path);
        let mut best: Option<usize> = None;
        let mut hits = Vec::new();
        for candidate in path.ancestors() {
            if candidate.as_os_str().is_empty() {
                continue;
            }
            self.set.matches_into(candidate, &mut hits);
            for &glob in &hits {
                let rule = self.owner[glob];
                if best.is_none_or(|b| rule < b) {
                    best = Some(rule);
                }
            }
            if best == Some(0) {
                break;
            }
        }
        best.map(|index| &self.list[index])
    }

    /// The first applicable rule when it is a skip rule.
    #[must_use]
    pub fn skipped_by(&self, relative_path: &Path) -> Option<&Rule> {
        self.matched(relative_path)
            .filter(|rule| rule.action == RuleAction::Skip)
    }

    /// Lowercase hex SHA-256 of the rules file bytes as given to
    /// [`Rules::from_toml`].
    #[must_use]
    pub fn hash_hex(&self) -> String {
        self.hash.clone()
    }

    /// The `version` declared by the rules file.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The rules in file order.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.list
    }
}

/// Strip a leading root and `.` components so `/Users/x` and `./Users/x`
/// match the same rules as `Users/x`.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    path.components()
        .filter(|c| matches!(c, Component::Normal(_) | Component::ParentDir))
        .collect()
}

/// Lowercase hex of a byte slice.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(rules: &Rules, path: &str) -> Option<String> {
        rules.skipped_by(Path::new(path)).map(|r| r.id.clone())
    }

    #[test]
    fn default_rules_parse() {
        let rules = Rules::default_backup();
        assert_eq!(rules.version(), 1);
        assert_eq!(rules.hash_hex().len(), 64);
        assert!(rules.rules().len() >= 15);
        assert!(rules.rules().iter().all(|r| r.action == RuleAction::Skip));
    }

    #[test]
    fn screenshot_files_are_skipped() {
        let rules = Rules::default_backup();
        let cases = [
            ("Users/josh/.colima/_lima/_disks/colima/datadisk", "colima-lima-vm"),
            (
                "Users/josh/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                "editor-state-db",
            ),
            (
                "Users/josh/Library/Application Support/Cursor/User/globalStorage/state.vscdb.backup",
                "editor-state-db",
            ),
            (
                "System/Library/AssetsV2/com_apple_MobileAsset_iOSSimulatorRuntime/x.asset/AssetData/a.dmg",
                "mobile-asset",
            ),
            ("Users/josh/.android/avd/x.avd/userdata-qemu.img.qcow2", "qcow2-image"),
        ];
        for (path, want) in cases {
            assert_eq!(id(&rules, path).as_deref(), Some(want), "{path}");
        }
    }

    #[test]
    fn every_default_category_skips() {
        let rules = Rules::default_backup();
        let cases = [
            ("Users/ann/.Trash/old.zip", "trash"),
            (".Trashes/501/x", "trash"),
            (".Spotlight-V100/Store-V2/x", "volume-metadata"),
            (".fseventsd/0001", "volume-metadata"),
            (".DocumentRevisions-V100/PerUID/x", "volume-metadata"),
            ("Users/ann/.lima/default/diffdisk", "colima-lima-vm"),
            (
                "Users/ann/Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw",
                "docker-desktop-vm",
            ),
            ("Volumes/x/Docker.raw", "docker-desktop-vm"),
            ("Users/ann/vm/disk.qcow2", "qcow2-image"),
            ("Users/ann/.android/avd/p.avd/cache.img", "android-avd"),
            ("Users/ann/.vagrant.d/boxes/u/0/box.vmdk", "vagrant-boxes"),
            (
                "Users/ann/Library/Developer/CoreSimulator/Devices/x/data.img",
                "ios-simulator",
            ),
            (
                "private/var/MobileAsset/AssetsV2/com_apple_MobileAsset_Font7/a",
                "mobile-asset",
            ),
            (
                "Users/ann/Library/Developer/Xcode/DerivedData/App/Index/x",
                "xcode-derived-data",
            ),
            ("Users/ann/Library/Caches/com.app/blob", "caches"),
            ("Library/Caches/x", "caches"),
            ("Users/ann/.cache/huggingface/model.bin", "caches"),
            ("Users/ann/Library/Application Support/Slack/Cache/x", "caches"),
            ("Users/ann/src/web/node_modules/x/index.js", "node-modules"),
            ("Users/ann/.pnpm-store/v3/files/x", "pnpm-store"),
            ("Users/ann/Library/pnpm/store/v3/x", "pnpm-store"),
            ("Users/ann/src/app/target/debug/app", "rust-target"),
            ("Users/ann/src/app/target/release/deps/x.rlib", "rust-target"),
            (
                "Users/ann/src/app/target/aarch64-apple-darwin/release/app",
                "rust-target",
            ),
            ("Users/ann/.cargo/registry/src/x/lib.rs", "cargo-registry"),
            ("Users/ann/py/.venv/lib/x.so", "python-venv"),
            ("Users/ann/py/venv/lib/x.so", "python-venv"),
            (
                "Users/ann/Library/Application Support/Claude/vm_bundles/claudevm.bundle/rootfs.img",
                "claude-vm-bundles",
            ),
            ("System/Library/Foo/bar", "system"),
            ("private/var/vm/sleepimage", "vm-swap"),
        ];
        for (path, want) in cases {
            assert_eq!(id(&rules, path).as_deref(), Some(want), "{path}");
        }
    }

    #[test]
    fn personal_files_are_kept() {
        let rules = Rules::default_backup();
        for path in [
            "Users/josh/Movies/a.mov",
            "Users/josh/Documents/taxes.pdf",
            "Users/josh/Pictures/Photos Library.photoslibrary",
            "Users/josh/VMs/Windows.vmwarevm/disk.vmdk",
            "Users/josh/src/app/target/notes.txt",
            "Users/josh/src/app/Cargo.toml",
            "Users/josh/Library/Mobile Documents/x.key",
            "Users/josh/cachet/x",
            "Users/josh/MyCaches.txt",
        ] {
            assert_eq!(id(&rules, path), None, "{path}");
        }
    }

    #[test]
    fn a_rule_on_a_directory_covers_the_directory_itself() {
        let rules = Rules::default_backup();
        assert_eq!(
            id(&rules, "Users/ann/src/node_modules").as_deref(),
            Some("node-modules")
        );
        assert_eq!(id(&rules, "Users/ann/.colima").as_deref(), Some("colima-lima-vm"));
    }

    #[test]
    fn leading_slash_and_dot_are_ignored() {
        let rules = Rules::default_backup();
        assert_eq!(id(&rules, "/Users/a/.Trash/x").as_deref(), Some("trash"));
        assert_eq!(id(&rules, "./Users/a/.Trash/x").as_deref(), Some("trash"));
    }

    #[test]
    fn include_above_skip_wins_and_below_does_not() {
        let text = r#"
version = 3
[[rule]]
id = "keep-models"
reason = "models are slow to fetch"
action = "include"
globs = ["**/.cache/models"]
[[rule]]
id = "caches"
reason = "caches"
action = "skip"
globs = ["**/.cache"]
[[rule]]
id = "late-include"
reason = "never reached"
action = "include"
globs = ["**/.cache/pip"]
"#;
        let rules = Rules::from_toml(text).unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(rules.version(), 3);
        let m = rules.matched(Path::new("Users/a/.cache/models/m.bin"));
        assert_eq!(m.map(|r| r.id.as_str()), Some("keep-models"));
        assert_eq!(rules.skipped_by(Path::new("Users/a/.cache/models/m.bin")), None);
        assert_eq!(id(&rules, "Users/a/.cache/pip/w.whl").as_deref(), Some("caches"));
        assert_eq!(rules.matched(Path::new("Users/a/Movies/x")), None);
    }

    #[test]
    fn star_does_not_cross_a_separator() {
        let text = r#"
version = 1
[[rule]]
id = "home-tmp"
reason = "r"
action = "skip"
globs = ["Users/*/tmp"]
"#;
        let rules = Rules::from_toml(text).unwrap_or_else(|e| unreachable!("{e}"));
        assert!(id(&rules, "Users/a/tmp/x").is_some());
        assert!(id(&rules, "Users/a/b/tmp/x").is_none());
    }

    #[test]
    fn non_utf8_names_still_match() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let rules = Rules::default_backup();
        let mut path = PathBuf::from("Users/a/node_modules");
        path.push(OsStr::from_bytes(b"bad\xffname"));
        assert_eq!(rules.skipped_by(&path).map(|r| r.id.as_str()), Some("node-modules"));
    }

    #[test]
    fn hash_tracks_the_bytes() {
        let a = "version = 1\n";
        let b = "version = 1\n\n";
        let ra = Rules::from_toml(a).unwrap_or_else(|e| unreachable!("{e}"));
        let rb = Rules::from_toml(b).unwrap_or_else(|e| unreachable!("{e}"));
        assert_ne!(ra.hash_hex(), rb.hash_hex());
        assert_eq!(
            ra.hash_hex(),
            Rules::from_toml(a).unwrap_or_else(|e| unreachable!("{e}")).hash_hex()
        );
    }

    #[test]
    fn bad_files_are_refused() {
        let base = |body: &str| format!("version = 1\n[[rule]]\n{body}\n");
        assert!(matches!(Rules::from_toml("version = 0"), Err(RulesError::Version)));
        assert!(matches!(Rules::from_toml("rule = 3"), Err(RulesError::Parse(_))));
        assert!(matches!(
            Rules::from_toml(&base(
                "id = \"a\"\nreason = \"r\"\naction = \"skip\"\nglobs = [\"x\"]\nextra = 1"
            )),
            Err(RulesError::Parse(_))
        ));
        assert!(matches!(
            Rules::from_toml(&base(
                "id = \"a\"\nreason = \"r\"\naction = \"delete\"\nglobs = [\"x\"]"
            )),
            Err(RulesError::Parse(_))
        ));
        assert!(matches!(
            Rules::from_toml(&base("id = \"\"\nreason = \"r\"\naction = \"skip\"\nglobs = [\"x\"]")),
            Err(RulesError::Incomplete { .. })
        ));
        assert!(matches!(
            Rules::from_toml(&base("id = \"a\"\nreason = \" \"\naction = \"skip\"\nglobs = [\"x\"]")),
            Err(RulesError::Incomplete { .. })
        ));
        assert!(matches!(
            Rules::from_toml(&base("id = \"a\"\nreason = \"r\"\naction = \"skip\"\nglobs = []")),
            Err(RulesError::Incomplete { .. })
        ));
        assert!(matches!(
            Rules::from_toml(&base("id = \"a\"\nreason = \"r\"\naction = \"skip\"\nglobs = [\"a[\"]")),
            Err(RulesError::Glob { .. })
        ));
        let dup = "version = 1\n[[rule]]\nid = \"a\"\nreason = \"r\"\naction = \"skip\"\nglobs = [\"x\"]\n[[rule]]\nid = \"a\"\nreason = \"r\"\naction = \"skip\"\nglobs = [\"y\"]\n";
        assert!(matches!(Rules::from_toml(dup), Err(RulesError::DuplicateId(_))));
    }
}
