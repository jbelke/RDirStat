//! Backup profiles: what one recurring Backup copies, stored on the
//! destination drive so the drive describes its own jobs.
//!
//! A profile lives at `<dest>/.rdirstat/profiles/<name>.toml` ([`profile_path`]).
//! It names the destination, one or more sources (each a backup label and a
//! volume root, optionally narrowed to directories and single files under the
//! root), a rules file, the floor, and the review cap. A job is one instance
//! of a profile: `rdirstat backup plan --profile <path>` plans every source.
//!
//! ```toml
//! schema_version = 1
//! name = "laptop"
//! dest = "/Volumes/gdr22tb"
//! rules = "rules.toml"          # optional; relative to the profile's directory
//! floor_allocated = 104857600   # optional; default 100 MiB
//! review_cap = 50000            # optional; default 50 000
//!
//! [[sources]]
//! label = "mac-data"
//! root = "/System/Volumes/Data"
//! include = ["Users/josh/Documents", "Users/josh/disk.iso"]
//! ```
//!
//! This module is pure like the rest of the crate: it parses, renders, and
//! checks what can be checked without a filesystem. The shell resolves paths,
//! reads the rules file, compares disks, and writes the file atomically.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fit::{DEFAULT_CAP, DEFAULT_FLOOR_ALLOCATED};
use crate::layout::validate_label;

/// The profile format this build reads and writes.
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

/// Directory under the destination root that holds profiles.
pub const PROFILES_DIR: &str = ".rdirstat/profiles";

/// Largest review cap a profile may ask for. Each ranked candidate is held in
/// memory and listed in the plan file; past this a plan is not reviewable.
pub const MAX_REVIEW_CAP: usize = 1_000_000;

/// One backup profile, as stored in TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Must equal [`PROFILE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Profile name; also the file stem. Same alphabet as a source label.
    pub name: String,
    /// Destination root: the backup volume's mount point.
    pub dest: PathBuf,
    /// Skip/include rules file. Relative paths are resolved against the
    /// directory holding the profile. Absent: the built-in Backup rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<PathBuf>,
    /// Smallest allocated size a candidate may have, in bytes.
    #[serde(default = "default_floor")]
    pub floor_allocated: u64,
    /// Review cap on ranked candidates per source.
    #[serde(default = "default_cap")]
    pub review_cap: usize,
    /// What to back up. At least one.
    pub sources: Vec<ProfileSource>,
}

/// One source of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSource {
    /// Name under `<dest>/Backups/`, e.g. `mac-data`.
    pub label: String,
    /// Source volume root. Mirror paths are relative to it.
    pub root: PathBuf,
    /// Directories and single files under `root` to back up, relative to
    /// `root` or absolute under it. Empty: the whole root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<PathBuf>,
}

const fn default_floor() -> u64 {
    DEFAULT_FLOOR_ALLOCATED
}

const fn default_cap() -> usize {
    DEFAULT_CAP
}

/// Why a profile was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// The TOML did not parse into a profile.
    #[error("profile is not valid TOML for schema {PROFILE_SCHEMA_VERSION}: {0}")]
    Parse(String),
    /// The profile could not be rendered as TOML.
    #[error("profile cannot be written as TOML: {0}")]
    Render(String),
    /// `schema_version` is not one this build reads.
    #[error("profile schema_version {0} is not supported (this build reads {PROFILE_SCHEMA_VERSION})")]
    Schema(u32),
    /// The name is outside the label alphabet.
    #[error("profile name {0:?} is not allowed: use 1-64 of A-Z a-z 0-9 . _ - and do not start with '.'")]
    Name(String),
    /// A path that must be absolute is not.
    #[error("{what} {path:?} must be an absolute path")]
    NotAbsolute {
        /// Which field.
        what: &'static str,
        /// The value given.
        path: PathBuf,
    },
    /// The profile names no source.
    #[error("profile names no source")]
    NoSources,
    /// A source label is outside the label alphabet.
    #[error("source label {0:?} is not allowed: use 1-64 of A-Z a-z 0-9 . _ - and do not start with '.'")]
    Label(String),
    /// Two sources share a label, so they would share one mirror.
    #[error("source label {0:?} is used twice; each source needs its own mirror")]
    DuplicateLabel(String),
    /// An include is empty, climbs with `..`, or is absolute outside the root.
    #[error("include {include:?} of source {label:?} is not under its root {root:?}")]
    IncludeOutsideRoot {
        /// The source's label.
        label: String,
        /// The source's root.
        root: PathBuf,
        /// The include as written.
        include: PathBuf,
    },
    /// The review cap is zero or above [`MAX_REVIEW_CAP`].
    #[error("review_cap {0} must be between 1 and {MAX_REVIEW_CAP}")]
    ReviewCap(usize),
}

impl Profile {
    /// Parses and [`validate`](Self::validate)s a profile.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Parse`] for bad TOML or unknown fields, or any
    /// validation error.
    pub fn from_toml(text: &str) -> Result<Profile, ProfileError> {
        let profile: Profile = toml::from_str(text).map_err(|error| ProfileError::Parse(error.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Renders the profile as TOML after validating it.
    ///
    /// # Errors
    ///
    /// Any validation error, or [`ProfileError::Render`].
    pub fn to_toml(&self) -> Result<String, ProfileError> {
        self.validate()?;
        toml::to_string(self).map_err(|error| ProfileError::Render(error.to_string()))
    }

    /// Checks everything that needs no filesystem: schema, name, labels,
    /// absolute dest and roots, includes lexically under their root, and the
    /// review cap. Disk identity and existence are the shell's to check.
    ///
    /// # Errors
    ///
    /// The first [`ProfileError`] found.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.schema_version != PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::Schema(self.schema_version));
        }
        if validate_label(&self.name).is_err() {
            return Err(ProfileError::Name(self.name.clone()));
        }
        if !self.dest.is_absolute() {
            return Err(ProfileError::NotAbsolute {
                what: "dest",
                path: self.dest.clone(),
            });
        }
        if self.review_cap == 0 || self.review_cap > MAX_REVIEW_CAP {
            return Err(ProfileError::ReviewCap(self.review_cap));
        }
        if self.sources.is_empty() {
            return Err(ProfileError::NoSources);
        }
        let mut labels = std::collections::HashSet::new();
        for source in &self.sources {
            if validate_label(&source.label).is_err() {
                return Err(ProfileError::Label(source.label.clone()));
            }
            if !labels.insert(source.label.as_str()) {
                return Err(ProfileError::DuplicateLabel(source.label.clone()));
            }
            if !source.root.is_absolute() {
                return Err(ProfileError::NotAbsolute {
                    what: "source root",
                    path: source.root.clone(),
                });
            }
            for include in &source.include {
                if include_relative(&source.root, include).is_none() {
                    return Err(ProfileError::IncludeOutsideRoot {
                        label: source.label.clone(),
                        root: source.root.clone(),
                        include: include.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// `include` as a path relative to `root`, lexically: an absolute include
/// must start with `root`; the result is non-empty, has no `..`, and `.`
/// components are dropped. `None` when the include is not under the root.
///
/// Lexical only: a symlink inside the root can still point outside it, which
/// the shell catches after resolving.
#[must_use]
pub fn include_relative(root: &Path, include: &Path) -> Option<PathBuf> {
    let relative = if include.is_absolute() {
        include.strip_prefix(root).ok()?
    } else {
        include
    };
    let mut out = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// `<dest>/.rdirstat/profiles/<name>.toml`.
#[must_use]
pub fn profile_path(dest: &Path, name: &str) -> PathBuf {
    dest.join(PROFILES_DIR).join(format!("{name}.toml"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

    use super::*;

    const SAMPLE: &str = r#"
schema_version = 1
name = "laptop"
dest = "/Volumes/backup"
rules = "rules.toml"

[[sources]]
label = "mac-data"
root = "/System/Volumes/Data"
include = ["Users/josh/Documents", "/System/Volumes/Data/Users/josh/disk.iso"]

[[sources]]
label = "tuf8tb"
root = "/Volumes/tuf8tb"
"#;

    #[test]
    fn a_profile_parses_with_defaults_and_round_trips() {
        let profile = Profile::from_toml(SAMPLE).expect("parse");
        assert_eq!(profile.floor_allocated, DEFAULT_FLOOR_ALLOCATED);
        assert_eq!(profile.review_cap, DEFAULT_CAP);
        assert_eq!(profile.sources.len(), 2);
        assert!(profile.sources[1].include.is_empty());
        let again = Profile::from_toml(&profile.to_toml().expect("render")).expect("reparse");
        assert_eq!(again, profile);
    }

    #[test]
    fn includes_resolve_relative_to_the_root() {
        let root = Path::new("/r");
        assert_eq!(include_relative(root, Path::new("a/./b")), Some(PathBuf::from("a/b")));
        assert_eq!(include_relative(root, Path::new("/r/a")), Some(PathBuf::from("a")));
        assert_eq!(include_relative(root, Path::new("/other/a")), None);
        assert_eq!(include_relative(root, Path::new("a/../../x")), None);
        assert_eq!(include_relative(root, Path::new("/r")), None);
        assert_eq!(include_relative(root, Path::new(".")), None);
    }

    #[test]
    fn bad_profiles_are_refused() {
        type Case = (&'static str, &'static str, fn(&ProfileError) -> bool);
        let cases: &[Case] = &[
            ("schema_version = 1", "schema_version = 2", |e| {
                matches!(e, ProfileError::Schema(2))
            }),
            ("name = \"laptop\"", "name = \".hidden\"", |e| {
                matches!(e, ProfileError::Name(_))
            }),
            ("dest = \"/Volumes/backup\"", "dest = \"relative\"", |e| {
                matches!(e, ProfileError::NotAbsolute { .. })
            }),
            ("rules = \"rules.toml\"", "review_cap = 0", |e| {
                matches!(e, ProfileError::ReviewCap(0))
            }),
            ("rules = \"rules.toml\"", "bogus = 1", |e| {
                matches!(e, ProfileError::Parse(_))
            }),
        ];
        for (from, to, check) in cases {
            let text = SAMPLE.replacen(from, to, 1);
            let error = Profile::from_toml(&text).expect_err(to);
            assert!(check(&error), "{to}: {error}");
        }
    }

    #[test]
    fn source_problems_are_refused() {
        let mut profile = Profile::from_toml(SAMPLE).expect("parse");
        profile.sources[1].label = "mac-data".into();
        assert!(matches!(profile.validate(), Err(ProfileError::DuplicateLabel(_))));

        let mut profile = Profile::from_toml(SAMPLE).expect("parse");
        profile.sources[0].include.push(PathBuf::from("../etc"));
        assert!(matches!(
            profile.validate(),
            Err(ProfileError::IncludeOutsideRoot { .. })
        ));

        let mut profile = Profile::from_toml(SAMPLE).expect("parse");
        profile.sources.clear();
        assert!(matches!(profile.validate(), Err(ProfileError::NoSources)));
    }

    #[test]
    fn the_profile_path_is_on_the_drive() {
        assert_eq!(
            profile_path(Path::new("/Volumes/b"), "laptop"),
            PathBuf::from("/Volumes/b/.rdirstat/profiles/laptop.toml")
        );
    }
}
