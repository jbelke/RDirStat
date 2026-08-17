//! The configuration surface: what a category *is*, what a valid table looks
//! like, and every way a candidate configuration can be rejected.
//!
//! Parsing produces a candidate; only a fully valid candidate is compiled and
//! swapped in (docs/04-CLASSIFICATION.md#configuration-lifecycle). Nothing here
//! runs during a scan.

use serde::{Deserialize, Serialize};

use crate::matcher::GlobScope;
use crate::tags::ContextTag;

pub use rdirstat_core::CategoryId;

/// The upper bound on multi-part suffixes a configuration may ask for.
///
/// `tar.gz` is two parts; `tar.gz.gpg` is three. Beyond a handful this stops
/// being an extension protocol and starts being a path parser, and every extra
/// part costs a backwards scan on every name.
pub const MAX_SUFFIX_PARTS: u8 = 8;

/// The stable key of the mandatory "no idea" category, which is always id 0.
pub const UNCATEGORIZED_KEY: &str = "uncategorized";
/// The stable key of the mandatory symlink category.
pub const SYMLINK_KEY: &str = "symlink";
/// The stable key of the mandatory executable category.
pub const EXECUTABLE_KEY: &str = "executable";

/// An 8-bit-per-channel colour.
///
/// This is *configuration metadata*, not wire data: the backend sends category
/// indices and the frontend resolves colours from CSS variables
/// (docs/05-UI.md). It lives here so the settings surface and an exported
/// configuration file round-trip losslessly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// A colour from its three channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#rrggbb` (the leading `#` is optional, case-insensitive).
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidColor`] if the string is not six hex digits.
    pub fn from_hex(text: &str) -> Result<Self, ConfigError> {
        let body = text.strip_prefix('#').unwrap_or(text);
        let invalid = || ConfigError::InvalidColor { value: text.to_owned() };
        if body.len() != 6 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let channel = |range: core::ops::Range<usize>| -> Result<u8, ConfigError> {
            body.get(range)
                .and_then(|slice| u8::from_str_radix(slice, 16).ok())
                .ok_or_else(invalid)
        };
        Ok(Self::new(channel(0..2)?, channel(2..4)?, channel(4..6)?))
    }

    /// Renders `#rrggbb` in lowercase.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl TryFrom<String> for Rgb {
    type Error = ConfigError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_hex(&value)
    }
}

impl From<Rgb> for String {
    fn from(value: Rgb) -> Self {
        value.to_hex()
    }
}

/// One basename pattern in a candidate configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobSpec {
    /// The pattern: `*` matches any run of bytes, `?` any single byte.
    pub pattern: String,
    /// Whether the pattern is matched byte-for-byte. Default: ASCII-folded.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Which entry kinds the pattern may match. Default: [`GlobScope::Any`].
    #[serde(default)]
    pub scope: GlobScope,
}

impl GlobSpec {
    /// A case-insensitive pattern that matches files and directories.
    #[must_use]
    pub fn any(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
            case_sensitive: false,
            scope: GlobScope::Any,
        }
    }

    /// A case-insensitive pattern restricted to directories.
    #[must_use]
    pub fn directories(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
            case_sensitive: false,
            scope: GlobScope::Directories,
        }
    }

    /// A byte-exact pattern restricted to files.
    #[must_use]
    pub fn exact_file(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
            case_sensitive: true,
            scope: GlobScope::Files,
        }
    }

    /// A byte-exact pattern that matches files and directories.
    #[must_use]
    pub fn exact_any(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
            case_sensitive: true,
            scope: GlobScope::Any,
        }
    }
}

/// One category in a candidate configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategorySpec {
    /// Stable, untranslated persistence key. Unique within a configuration.
    pub key: String,
    /// User-visible label. Translatable; never persisted as an identity.
    pub label: String,
    /// Swatch for the settings surface.
    pub color: Rgb,
    /// Whether a *directory* may carry this category.
    ///
    /// This is the mechanism that keeps `Library/Caches/movie.mp4` a Video:
    /// `cache` and `package` are directory-only categories, so they cannot
    /// swallow a file (docs/04-CLASSIFICATION.md#two-outputs-not-one-overloaded-category).
    #[serde(default)]
    pub directory_eligible: bool,
    /// ASCII-folded suffixes, written without the leading dot (`tar.gz`).
    #[serde(default)]
    pub suffixes_ci: Vec<String>,
    /// Byte-exact suffixes, tried before the folded ones at each length.
    #[serde(default)]
    pub suffixes_cs: Vec<String>,
    /// Ordered basename patterns, evaluated only after every suffix misses.
    #[serde(default)]
    pub basename_globs: Vec<GlobSpec>,
    /// Context tags a *directory* of this category contributes to its subtree.
    #[serde(default)]
    pub implies: Vec<ContextTag>,
}

impl CategorySpec {
    /// A category with no matching rules at all.
    #[must_use]
    pub fn new(key: &str, label: &str, color: Rgb) -> Self {
        Self {
            key: key.to_owned(),
            label: label.to_owned(),
            color,
            directory_eligible: false,
            suffixes_ci: Vec::new(),
            suffixes_cs: Vec::new(),
            basename_globs: Vec::new(),
            implies: Vec::new(),
        }
    }

    /// Adds ASCII-folded suffixes.
    #[must_use]
    pub fn with_suffixes(mut self, suffixes: &[&str]) -> Self {
        self.suffixes_ci.extend(suffixes.iter().map(|s| (*s).to_owned()));
        self
    }

    /// Adds byte-exact suffixes.
    #[must_use]
    pub fn with_exact_suffixes(mut self, suffixes: &[&str]) -> Self {
        self.suffixes_cs.extend(suffixes.iter().map(|s| (*s).to_owned()));
        self
    }

    /// Adds ordered basename patterns.
    #[must_use]
    pub fn with_globs(mut self, globs: Vec<GlobSpec>) -> Self {
        self.basename_globs.extend(globs);
        self
    }

    /// Marks the category as one a directory may carry.
    #[must_use]
    pub const fn directories_too(mut self) -> Self {
        self.directory_eligible = true;
        self
    }

    /// Declares the context tags a directory of this category implies.
    #[must_use]
    pub fn implying(mut self, tags: &[ContextTag]) -> Self {
        self.implies.extend_from_slice(tags);
        self
    }
}

/// A directory-component rule for context tagging.
///
/// Component-aware and literal: a directory named exactly `Caches` matches,
/// `MyCachesBackup` does not (docs/04-CLASSIFICATION.md#context-tagging).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentRule {
    /// The exact component name, matched ASCII-folded.
    pub name: String,
    /// The tags the component contributes to its whole subtree.
    pub tags: Vec<ContextTag>,
}

impl ComponentRule {
    /// A rule for one component name.
    #[must_use]
    pub fn new(name: &str, tags: &[ContextTag]) -> Self {
        Self {
            name: name.to_owned(),
            tags: tags.to_vec(),
        }
    }
}

/// A candidate configuration, before validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryConfig {
    /// Format version of this document. Bumped when the shape changes.
    pub schema_version: u16,
    /// The longest multi-part suffix to try, in dot-separated parts.
    pub max_suffix_parts: u8,
    /// Ordered categories. Index 0 must be the uncategorized category, and the
    /// declaration order fixes both the `CategoryId` values and the order in
    /// which basename globs are evaluated.
    pub categories: Vec<CategorySpec>,
    /// Directory-component rules for context tagging.
    #[serde(default)]
    pub components: Vec<ComponentRule>,
}

impl Default for CategoryConfig {
    fn default() -> Self {
        crate::defaults::default_config()
    }
}

/// A compiled, validated category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    id: CategoryId,
    key: Box<str>,
    label: Box<str>,
    color: Rgb,
    directory_eligible: bool,
    implies: crate::tags::ContextTags,
}

impl Category {
    pub(crate) fn new(
        id: CategoryId,
        key: &str,
        label: &str,
        color: Rgb,
        directory_eligible: bool,
        implies: crate::tags::ContextTags,
    ) -> Self {
        Self {
            id,
            key: key.into(),
            label: label.into(),
            color,
            directory_eligible,
            implies,
        }
    }

    /// The index stored in [`rdirstat_core::Node::category`].
    #[must_use]
    pub const fn id(&self) -> CategoryId {
        self.id
    }

    /// The stable, untranslated persistence key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The user-visible label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The settings-surface swatch.
    #[must_use]
    pub const fn color(&self) -> Rgb {
        self.color
    }

    /// Whether a directory may carry this category.
    #[must_use]
    pub const fn directory_eligible(&self) -> bool {
        self.directory_eligible
    }

    /// The context tags a directory of this category implies for its subtree.
    #[must_use]
    pub const fn implies(&self) -> crate::tags::ContextTags {
        self.implies
    }
}

/// Every way a candidate configuration can be rejected.
///
/// Compilation is all-or-nothing: an invalid candidate never replaces a
/// working table, so a bad user overlay degrades to "settings show an error",
/// never to "half the scan is uncategorized".
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The category list was empty.
    #[error("a configuration must define at least the mandatory categories")]
    NoCategories,

    /// More categories than a `u8` id can address.
    #[error("{count} categories exceeds the limit of {limit}")]
    TooManyCategories {
        /// How many were declared.
        count: usize,
        /// The ceiling, from [`CategoryId::MAX_CATEGORIES`].
        limit: usize,
    },

    /// Two categories shared a key.
    #[error("duplicate category key `{key}`")]
    DuplicateCategoryKey {
        /// The repeated key.
        key: String,
    },

    /// A category key or label was empty.
    #[error("category at index {index} has an empty {field}")]
    EmptyCategoryField {
        /// Position in the declared list.
        index: usize,
        /// Which field: `key` or `label`.
        field: &'static str,
    },

    /// Index 0 was not the uncategorized category.
    #[error("category 0 must be `{UNCATEGORIZED_KEY}`, found `{found}`")]
    UncategorizedNotFirst {
        /// The key that was found at index 0.
        found: String,
    },

    /// A mandatory category was missing.
    #[error("mandatory category `{key}` is missing")]
    MissingRequiredCategory {
        /// The missing key.
        key: &'static str,
    },

    /// A mandatory category declared matching rules it must not have.
    #[error("category `{key}` is assigned by rule, not by name, and must declare no patterns")]
    RuleFreeCategoryHasPatterns {
        /// The offending key.
        key: &'static str,
    },

    /// The same suffix was claimed by two categories in the same map.
    #[error("suffix `{suffix}` is claimed by both `{first}` and `{second}`")]
    DuplicateSuffix {
        /// The repeated suffix.
        suffix: String,
        /// The category that claimed it first.
        first: String,
        /// The category that claimed it again.
        second: String,
    },

    /// The same basename pattern was declared twice.
    #[error("basename pattern `{pattern}` is declared twice (`{first}`, `{second}`)")]
    DuplicateGlob {
        /// The repeated pattern.
        pattern: String,
        /// The category that declared it first.
        first: String,
        /// The category that declared it again.
        second: String,
    },

    /// A suffix was written with a leading dot.
    #[error("suffix `{suffix}` must be written without a leading dot")]
    SuffixLeadingDot {
        /// The offending suffix.
        suffix: String,
    },

    /// A suffix was empty, or ended in a dot.
    #[error("category `{key}` declares an empty suffix component")]
    EmptySuffix {
        /// The category that declared it.
        key: String,
    },

    /// A suffix had more dot-separated parts than `max_suffix_parts`, so the
    /// matcher could never reach it.
    #[error("suffix `{suffix}` has {parts} parts but max_suffix_parts is {max}; it can never match")]
    UnreachableSuffix {
        /// The offending suffix.
        suffix: String,
        /// Its part count.
        parts: usize,
        /// The configured maximum.
        max: u8,
    },

    /// `max_suffix_parts` was zero or above [`MAX_SUFFIX_PARTS`].
    #[error("max_suffix_parts must be between 1 and {limit}, found {value}")]
    InvalidMaxSuffixParts {
        /// The configured value.
        value: u8,
        /// The ceiling.
        limit: u8,
    },

    /// A basename pattern was empty.
    #[error("category `{key}` declares an empty basename pattern")]
    EmptyGlob {
        /// The category that declared it.
        key: String,
    },

    /// A colour string was not `#rrggbb`.
    #[error("`{value}` is not a #rrggbb colour")]
    InvalidColor {
        /// The offending string.
        value: String,
    },

    /// A component rule named the empty string.
    #[error("a component rule has an empty name")]
    EmptyComponentRule,

    /// Two component rules named the same directory.
    #[error("duplicate component rule for `{name}`")]
    DuplicateComponentRule {
        /// The repeated component name.
        name: String,
    },

    /// A lookup key was empty. Structural; a validated configuration cannot
    /// produce it.
    #[error("a lookup table key was empty")]
    EmptyKey,

    /// A key exceeded the stack fold buffer, which would make the folded probe
    /// unreachable for that key.
    #[error("key `{key}` is longer than the {limit}-byte fold buffer")]
    KeyTooLong {
        /// The offending key.
        key: String,
        /// The buffer size.
        limit: usize,
    },

    /// The compiled key blob or slot array overflowed its index type.
    #[error("the compiled lookup table overflowed its index type")]
    TableOverflow,
}

/// The schema version this build writes and understands.
pub const SCHEMA_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::{ConfigError, Rgb};

    #[test]
    fn colour_round_trips() {
        let colour = Rgb::from_hex("#4B78D1").expect("valid colour");
        assert_eq!(colour, Rgb::new(0x4b, 0x78, 0xd1));
        assert_eq!(colour.to_hex(), "#4b78d1");
        assert_eq!(Rgb::from_hex("4b78d1").expect("valid without hash"), colour);
    }

    #[test]
    fn colour_rejects_garbage() {
        for bad in ["#fff", "#gggggg", "", "#4b78d1ff", "4b78d"] {
            assert!(
                matches!(Rgb::from_hex(bad), Err(ConfigError::InvalidColor { .. })),
                "accepted {bad}"
            );
        }
    }
}
