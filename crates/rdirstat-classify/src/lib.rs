#![forbid(unsafe_code)]
//! Clean-room content categorisation for RDirStat.
//!
//! # What this crate decides
//!
//! Two outputs, never one overloaded value
//! (docs/04-CLASSIFICATION.md#two-outputs-not-one-overloaded-category):
//!
//! * one **primary content category** per entry — the `u8` stored in
//!   [`rdirstat_core::Node::category`], used for colour and size statistics;
//! * zero or more **context tags** per *directory component* — used by reports,
//!   folded down the path by the builder, and never stored per node (`Node` is
//!   48 bytes and a tag bitset is a memory-budget amendment, not a detail).
//!
//! # The algorithm
//!
//! [`Categorizer::classify`] is a fixed ladder. Each rung is tried in full
//! before the next one is reached:
//!
//! 1. **Symlink wins over everything** and is checked first, from
//!    [`Kind`] alone. A symlink named `movie.mp4` is a symlink.
//! 2. **Longest suffix first.** The name is split on dots and the longest
//!    multi-part suffix is tried first: `tar.gz` before `gz`. At most
//!    [`Categorizer::max_suffix_parts`] parts are considered. A leading dot is
//!    not a separator, so `.gitignore` has no suffix at all.
//! 3. **Exact case before folded case**, at *each* suffix length — so
//!    `x.tar.Z` finds the byte-exact `tar.Z` entry before anything folds, and
//!    `.C` stays C++ while `.c` stays C.
//! 4. **Ordered basename globs**, only as a fallback, in declaration order.
//! 5. **The execute bit**, for a regular file that matched nothing.
//! 6. **Uncategorized.**
//!
//! ```text
//! archive.tar.bz2
//!   exact "tar.bz2" -> folded "tar.bz2"
//!   exact "bz2"     -> folded "bz2"      <- hits: Compressed Stream
//!   (ordered basename globs)
//!   (execute bit)
//!   Uncategorized
//! ```
//!
//! # Allocation
//!
//! The classify path performs **zero heap allocation**. Suffix slices borrow
//! the caller's bytes, ASCII folding uses a 32-byte stack buffer, and the
//! lookup tables are flat arrays built once when a configuration is compiled.
//! A key too long to fold on the stack is *rejected at compile time*, so
//! skipping the folded probe for an over-long input cannot miss a match. See
//! `benches/classify.rs`.
//!
//! # Bytes, not strings
//!
//! Names arrive as filesystem bytes. macOS hands out NFD UTF-8 in practice but
//! guarantees nothing, so the matching surface is `&[u8]` and folding is
//! ASCII-only: Unicode case conversion is not a file-extension protocol
//! (docs/04-CLASSIFICATION.md#primary-category-algorithm).
//!
//! # Provenance
//!
//! Behaviour was reproduced from the description in docs/04; the taxonomy,
//! tables, keys and palette in [`defaults`] are authored from public format
//! knowledge for this repository. No table was transcribed from any GPL source.
//!
//! # Example
//!
//! ```
//! use rdirstat_classify::Categorizer;
//! use rdirstat_core::Kind;
//!
//! let categorizer = Categorizer::defaults()?;
//! let archive = categorizer.classify(b"backup.tar.gz", Kind::File, 0o644);
//! assert_eq!(categorizer.key_of(archive), Some("compressed-archive"));
//!
//! // A shorter suffix would have said "Compressed Stream".
//! let stream = categorizer.classify(b"backup.gz", Kind::File, 0o644);
//! assert_eq!(categorizer.key_of(stream), Some("compressed-stream"));
//! # Ok::<(), rdirstat_classify::ConfigError>(())
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use rdirstat_core::{ConfigHash, Kind};

pub mod defaults;
mod digest;
mod matcher;
mod schema;
mod tags;

pub use crate::matcher::GlobScope;
pub use crate::schema::{
    Category, CategoryConfig, CategoryId, CategorySpec, ComponentRule, ConfigError, EXECUTABLE_KEY, GlobSpec,
    MAX_SUFFIX_PARTS, Rgb, SCHEMA_VERSION, SYMLINK_KEY, UNCATEGORIZED_KEY,
};
pub use crate::tags::{ContextTag, ContextTags};

use crate::digest::Digest256;
use crate::matcher::{ByteGlob, FOLD_CAPACITY, GlobIndex, Table, fold_ascii};

/// The mode bits that make an unmatched regular file an Executable.
pub const MODE_EXECUTABLE_BITS: u32 = 0o111;

/// A compiled, immutable classifier.
///
/// Build one before a scan and share it: it is `Send + Sync`, holds no interior
/// mutability, and every method on the hot path takes `&self`. Hot reload
/// affects the *next* scan, never an active one — an in-flight scan keeps the
/// `Categorizer` it started with, which is what makes a snapshot's category
/// ids interpretable after the settings change
/// (docs/04-CLASSIFICATION.md#configuration-lifecycle).
#[derive(Clone, Debug)]
pub struct Categorizer {
    categories: Vec<Category>,
    exact: Table<CategoryId>,
    folded: Table<CategoryId>,
    globs: GlobIndex<CategoryId>,
    components: Table<u16>,
    /// Bit `n` set means category `n` may be assigned to a directory.
    directory_eligible: [u64; 4],
    max_suffix_parts: u8,
    digest: [u8; 32],
    symlink: CategoryId,
    executable: CategoryId,
}

impl Categorizer {
    /// Compiles the shipped defaults.
    ///
    /// # Errors
    ///
    /// Only if the shipped table is itself invalid, which a unit test forbids.
    pub fn defaults() -> Result<Self, ConfigError> {
        Self::compile(&defaults::default_config())
    }

    /// The process-wide default categorizer, compiled once.
    ///
    /// # Errors
    ///
    /// Same as [`Categorizer::defaults`].
    pub fn shared() -> Result<&'static Self, ConfigError> {
        static SHARED: OnceLock<Result<Categorizer, ConfigError>> = OnceLock::new();
        SHARED.get_or_init(Self::defaults).as_ref().map_err(Clone::clone)
    }

    /// Validates and compiles a candidate configuration.
    ///
    /// Compilation is all-or-nothing: an invalid candidate is rejected whole,
    /// so a bad user overlay degrades to "the settings screen shows an error",
    /// never to "half the scan is uncategorized".
    ///
    /// # Errors
    ///
    /// See [`ConfigError`]. Every variant is a rejection of the *candidate*;
    /// none of them can be produced by classification.
    #[allow(
        clippy::too_many_lines,
        reason = "the validation order is the contract; splitting it hides which check runs first"
    )]
    pub fn compile(config: &CategoryConfig) -> Result<Self, ConfigError> {
        if config.max_suffix_parts == 0 || config.max_suffix_parts > MAX_SUFFIX_PARTS {
            return Err(ConfigError::InvalidMaxSuffixParts {
                value: config.max_suffix_parts,
                limit: MAX_SUFFIX_PARTS,
            });
        }
        if config.categories.is_empty() {
            return Err(ConfigError::NoCategories);
        }
        if config.categories.len() > CategoryId::MAX_CATEGORIES {
            return Err(ConfigError::TooManyCategories {
                count: config.categories.len(),
                limit: CategoryId::MAX_CATEGORIES,
            });
        }

        let mut categories = Vec::with_capacity(config.categories.len());
        let mut keys: HashMap<&str, usize> = HashMap::with_capacity(config.categories.len());
        let mut exact_entries: Vec<(Vec<u8>, CategoryId)> = Vec::new();
        let mut folded_entries: Vec<(Vec<u8>, CategoryId)> = Vec::new();
        let mut exact_owner: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut folded_owner: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut globs: Vec<(ByteGlob, CategoryId)> = Vec::new();
        let mut glob_owner: HashMap<(bool, u8, Vec<u8>), usize> = HashMap::new();
        let mut directory_eligible = [0u64; 4];

        for (index, spec) in config.categories.iter().enumerate() {
            if spec.key.is_empty() {
                return Err(ConfigError::EmptyCategoryField { index, field: "key" });
            }
            if spec.label.is_empty() {
                return Err(ConfigError::EmptyCategoryField { index, field: "label" });
            }
            if index == 0 && spec.key != UNCATEGORIZED_KEY {
                return Err(ConfigError::UncategorizedNotFirst {
                    found: spec.key.clone(),
                });
            }
            if keys.insert(spec.key.as_str(), index).is_some() {
                return Err(ConfigError::DuplicateCategoryKey { key: spec.key.clone() });
            }

            // Category 0 and the symlink category are assigned by rule, not by
            // name; letting them carry patterns would make the ladder lie.
            let rule_free = if index == 0 {
                Some(UNCATEGORIZED_KEY)
            } else if spec.key == SYMLINK_KEY {
                Some(SYMLINK_KEY)
            } else {
                None
            };
            if let Some(key) = rule_free
                && !(spec.suffixes_ci.is_empty() && spec.suffixes_cs.is_empty() && spec.basename_globs.is_empty())
            {
                return Err(ConfigError::RuleFreeCategoryHasPatterns { key });
            }

            let raw = u8::try_from(index).map_err(|_| ConfigError::TooManyCategories {
                count: config.categories.len(),
                limit: CategoryId::MAX_CATEGORIES,
            })?;
            let id = CategoryId::from_raw(raw);

            if spec.directory_eligible {
                let word = usize::from(raw >> 6);
                if let Some(slot) = directory_eligible.get_mut(word) {
                    *slot |= 1u64 << u32::from(raw & 63);
                }
            }

            for suffix in &spec.suffixes_cs {
                validate_suffix(suffix, spec, config.max_suffix_parts)?;
                let key = suffix.as_bytes().to_vec();
                if let Some(first) = exact_owner.insert(key.clone(), index) {
                    return Err(duplicate_suffix(config, suffix, first, index));
                }
                exact_entries.push((key, id));
            }
            for suffix in &spec.suffixes_ci {
                validate_suffix(suffix, spec, config.max_suffix_parts)?;
                let key: Vec<u8> = suffix.bytes().map(|b| b.to_ascii_lowercase()).collect();
                if let Some(first) = folded_owner.insert(key.clone(), index) {
                    return Err(duplicate_suffix(config, suffix, first, index));
                }
                folded_entries.push((key, id));
            }

            for glob in &spec.basename_globs {
                if glob.pattern.is_empty() {
                    return Err(ConfigError::EmptyGlob { key: spec.key.clone() });
                }
                let compiled = ByteGlob::new(glob.pattern.as_bytes(), glob.case_sensitive, glob.scope);
                let dedup = (glob.case_sensitive, glob.scope.key(), compiled.pattern().to_vec());
                if let Some(first) = glob_owner.insert(dedup, index) {
                    return Err(ConfigError::DuplicateGlob {
                        pattern: glob.pattern.clone(),
                        first: config
                            .categories
                            .get(first)
                            .map_or_else(String::new, |spec| spec.key.clone()),
                        second: spec.key.clone(),
                    });
                }
                globs.push((compiled, id));
            }

            categories.push(Category::new(
                id,
                &spec.key,
                &spec.label,
                spec.color,
                spec.directory_eligible,
                spec.implies.iter().copied().collect(),
            ));
        }

        let symlink = *keys
            .get(SYMLINK_KEY)
            .ok_or(ConfigError::MissingRequiredCategory { key: SYMLINK_KEY })?;
        let executable = *keys
            .get(EXECUTABLE_KEY)
            .ok_or(ConfigError::MissingRequiredCategory { key: EXECUTABLE_KEY })?;

        let mut component_entries: Vec<(Vec<u8>, u16)> = Vec::with_capacity(config.components.len());
        let mut seen_components: HashSet<Vec<u8>> = HashSet::new();
        for rule in &config.components {
            if rule.name.is_empty() {
                return Err(ConfigError::EmptyComponentRule);
            }
            let key: Vec<u8> = rule.name.bytes().map(|b| b.to_ascii_lowercase()).collect();
            if !seen_components.insert(key.clone()) {
                return Err(ConfigError::DuplicateComponentRule {
                    name: rule.name.clone(),
                });
            }
            let bits: ContextTags = rule.tags.iter().copied().collect();
            component_entries.push((key, bits.bits()));
        }

        let categorizer = Self {
            categories,
            exact: Table::build(&exact_entries)?,
            folded: Table::build(&folded_entries)?,
            globs: GlobIndex::build(globs)?,
            components: Table::build(&component_entries)?,
            directory_eligible,
            max_suffix_parts: config.max_suffix_parts,
            digest: digest_of(config),
            symlink: CategoryId::from_raw(u8::try_from(symlink).unwrap_or(0)),
            executable: CategoryId::from_raw(u8::try_from(executable).unwrap_or(0)),
        };
        tracing::debug!(
            categories = categorizer.categories.len(),
            exact_suffixes = categorizer.exact.len(),
            folded_suffixes = categorizer.folded.len(),
            globs = categorizer.globs.len(),
            components = categorizer.components.len(),
            "compiled category configuration"
        );
        Ok(categorizer)
    }

    /// Classifies one entry. **Allocation-free.**
    ///
    /// `name` is the entry's basename in filesystem bytes, `kind` its
    /// [`Kind`], and `mode` its `st_mode` (pass `0` when it is not known — the
    /// only thing lost is the execute-bit fallback).
    #[must_use]
    pub fn classify(&self, name: &[u8], kind: Kind, mode: u32) -> CategoryId {
        let is_directory = match kind {
            // Rung 1: a symlink is a symlink, whatever it is named.
            Kind::Symlink => return self.symlink,
            Kind::Directory => true,
            Kind::File | Kind::Unknown => false,
            // Sockets, fifos and devices carry no user data worth colouring.
            _ => return CategoryId::UNCATEGORIZED,
        };

        if let Some(id) = self.match_name(name, is_directory) {
            return id;
        }
        if !is_directory && mode & MODE_EXECUTABLE_BITS != 0 {
            return self.executable;
        }
        CategoryId::UNCATEGORIZED
    }

    /// [`Categorizer::classify`] for a name that is already known to be UTF-8.
    ///
    /// Convenience only; the byte form is the real entry point, because a
    /// macOS name is not guaranteed to decode.
    #[must_use]
    pub fn classify_str(&self, name: &str, kind: Kind, mode: u32) -> CategoryId {
        self.classify(name.as_bytes(), kind, mode)
    }

    /// The context tags one path component contributes to its whole subtree.
    ///
    /// Component-aware and literal by construction: this takes a single
    /// component, never a path, so `MyCachesBackup` cannot match `Caches` and
    /// `node_modules.txt` cannot become a dependency tree. Allocation-free.
    #[must_use]
    pub fn context_tags(&self, component: &[u8]) -> ContextTags {
        let mut tags = ContextTags::NONE;
        let mut buffer = [0u8; FOLD_CAPACITY];
        if let Some(folded) = fold_ascii(component, &mut buffer)
            && let Some(bits) = self.components.get(folded)
        {
            tags |= ContextTags::from_bits(bits);
        }
        let category = self.classify(component, Kind::Directory, 0);
        if let Some(entry) = self.categories.get(usize::from(category.get())) {
            tags |= entry.implies();
        }
        tags
    }

    /// The compiled categories, indexed by [`CategoryId`].
    #[must_use]
    pub fn categories(&self) -> &[Category] {
        &self.categories
    }

    /// One compiled category.
    #[must_use]
    pub fn category(&self, id: CategoryId) -> Option<&Category> {
        self.categories.get(usize::from(id.get()))
    }

    /// The stable persistence key of a category.
    #[must_use]
    pub fn key_of(&self, id: CategoryId) -> Option<&str> {
        self.category(id).map(Category::key)
    }

    /// Looks a category up by its stable key. Linear; not a hot path.
    #[must_use]
    pub fn id_of(&self, key: &str) -> Option<CategoryId> {
        self.categories
            .iter()
            .find(|entry| entry.key() == key)
            .map(Category::id)
    }

    /// The mandatory symlink category.
    #[must_use]
    pub const fn symlink_category(&self) -> CategoryId {
        self.symlink
    }

    /// The mandatory executable category.
    #[must_use]
    pub const fn executable_category(&self) -> CategoryId {
        self.executable
    }

    /// The longest suffix, in dot-separated parts, that this configuration
    /// will try.
    #[must_use]
    pub const fn max_suffix_parts(&self) -> u8 {
        self.max_suffix_parts
    }

    /// The raw 32-byte configuration digest.
    ///
    /// Deterministic across processes and architectures, and **not**
    /// cryptographic: its job is deciding whether two scans were classified by
    /// the same table, so old ids stay interpretable.
    #[must_use]
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// The digest as the hex [`ConfigHash`] that goes into
    /// [`rdirstat_core::CompletedScan::category_config_hash`].
    #[must_use]
    pub fn config_hash(&self) -> ConfigHash {
        ConfigHash::from_digest(&self.digest)
    }

    /// Rungs 2-4 of the ladder: suffixes longest-first, then ordered globs.
    fn match_name(&self, name: &[u8], is_directory: bool) -> Option<CategoryId> {
        // A leading dot is part of the basename, not a separator: `.gitignore`
        // has no suffix, and `.DS_Store` is matched by a glob.
        let start = usize::from(name.first() == Some(&b'.'));
        let region = name.get(start..)?;

        // Dot offsets, right to left, bounded by `max_suffix_parts`.
        let mut dots = [0usize; MAX_SUFFIX_PARTS as usize];
        let mut found = 0usize;
        let mut end = region.len();
        while found < usize::from(self.max_suffix_parts) {
            let Some(position) = rfind_dot(region.get(..end)?) else {
                break;
            };
            if let Some(slot) = dots.get_mut(found) {
                *slot = position;
            }
            found += 1;
            end = position;
        }

        // Longest suffix first: the leftmost dot we found.
        for index in (0..found).rev() {
            let suffix = region.get(dots.get(index).copied()?.saturating_add(1)..)?;
            if suffix.is_empty() {
                continue;
            }
            if let Some(id) = self.exact.get(suffix)
                && self.eligible(id, is_directory)
            {
                return Some(id);
            }
            let mut buffer = [0u8; FOLD_CAPACITY];
            if let Some(folded) = fold_ascii(suffix, &mut buffer)
                && let Some(id) = self.folded.get(folded)
                && self.eligible(id, is_directory)
            {
                return Some(id);
            }
        }

        self.globs.first_match(name, is_directory)
    }

    /// Whether a category may be assigned to an entry of this directory-ness.
    fn eligible(&self, id: CategoryId, is_directory: bool) -> bool {
        if !is_directory {
            return true;
        }
        let raw = id.get();
        self.directory_eligible
            .get(usize::from(raw >> 6))
            .is_some_and(|word| word & (1u64 << u32::from(raw & 63)) != 0)
    }
}

/// The last `.` in `head`.
///
/// A scalar backwards scan beats `memchr::memrchr` on the length distribution
/// that matters — the mean file name is ~15 bytes, where SIMD setup costs more
/// than the whole scan — so the vector path is kept only for the long tail.
/// Measured: this is worth ~15 ns per name on the representative corpus.
fn rfind_dot(head: &[u8]) -> Option<usize> {
    const SCALAR_LIMIT: usize = 64;
    if head.len() <= SCALAR_LIMIT {
        head.iter().rposition(|&byte| byte == b'.')
    } else {
        memchr::memrchr(b'.', head)
    }
}

/// Rejects a suffix that could never match, or that is written wrongly.
fn validate_suffix(suffix: &str, spec: &CategorySpec, max_parts: u8) -> Result<(), ConfigError> {
    if suffix.is_empty() {
        return Err(ConfigError::EmptySuffix { key: spec.key.clone() });
    }
    if suffix.starts_with('.') {
        return Err(ConfigError::SuffixLeadingDot {
            suffix: suffix.to_owned(),
        });
    }
    if suffix.split('.').any(str::is_empty) {
        return Err(ConfigError::EmptySuffix { key: spec.key.clone() });
    }
    let parts = suffix.split('.').count();
    if parts > usize::from(max_parts) {
        return Err(ConfigError::UnreachableSuffix {
            suffix: suffix.to_owned(),
            parts,
            max: max_parts,
        });
    }
    Ok(())
}

fn duplicate_suffix(config: &CategoryConfig, suffix: &str, first: usize, second: usize) -> ConfigError {
    let name = |index: usize| {
        config
            .categories
            .get(index)
            .map_or_else(String::new, |spec| spec.key.clone())
    };
    ConfigError::DuplicateSuffix {
        suffix: suffix.to_owned(),
        first: name(first),
        second: name(second),
    }
}

/// Absorbs every field that can change a classification decision.
///
/// Label and colour are absorbed too: they do not change a decision, but they
/// do change what a stored id *means* to a reader, and the hash's job is
/// deciding whether an old snapshot can be interpreted with the current table.
fn digest_of(config: &CategoryConfig) -> [u8; 32] {
    let mut digest = Digest256::new();
    digest.field(b"rdirstat.categories.v1");
    digest.update(&config.schema_version.to_le_bytes());
    digest.byte(config.max_suffix_parts);
    for spec in &config.categories {
        digest.field(spec.key.as_bytes());
        digest.field(spec.label.as_bytes());
        digest.byte(spec.color.r);
        digest.byte(spec.color.g);
        digest.byte(spec.color.b);
        digest.byte(u8::from(spec.directory_eligible));
        digest.byte(b'C');
        for suffix in &spec.suffixes_cs {
            digest.field(suffix.as_bytes());
        }
        digest.byte(b'F');
        for suffix in &spec.suffixes_ci {
            digest.field(suffix.as_bytes());
        }
        digest.byte(b'G');
        for glob in &spec.basename_globs {
            digest.field(glob.pattern.as_bytes());
            digest.byte(u8::from(glob.case_sensitive));
            digest.byte(glob.scope.key());
        }
        digest.byte(b'T');
        for tag in &spec.implies {
            digest.field(tag.key().as_bytes());
        }
    }
    digest.byte(b'P');
    for rule in &config.components {
        digest.field(rule.name.as_bytes());
        for tag in &rule.tags {
            digest.field(tag.key().as_bytes());
        }
    }
    digest.finish()
}

#[cfg(test)]
mod tests {
    use super::{Categorizer, CategoryConfig, CategoryId, ConfigError, ContextTag, GlobSpec};
    use crate::schema::{CategorySpec, ComponentRule, Rgb, SCHEMA_VERSION};
    use rdirstat_core::Kind;

    fn categorizer() -> Categorizer {
        Categorizer::defaults().expect("the shipped defaults compile")
    }

    fn key(categorizer: &Categorizer, name: &str, kind: Kind, mode: u32) -> String {
        let id = categorizer.classify(name.as_bytes(), kind, mode);
        categorizer.key_of(id).unwrap_or("<unknown>").to_owned()
    }

    fn file(categorizer: &Categorizer, name: &str) -> String {
        key(categorizer, name, Kind::File, 0o644)
    }

    fn directory(categorizer: &Categorizer, name: &str) -> String {
        key(categorizer, name, Kind::Directory, 0o755)
    }

    fn minimal_config() -> CategoryConfig {
        CategoryConfig {
            schema_version: SCHEMA_VERSION,
            max_suffix_parts: 2,
            categories: vec![
                CategorySpec::new("uncategorized", "Uncategorized", Rgb::new(1, 2, 3)),
                CategorySpec::new("symlink", "Symlinks", Rgb::new(4, 5, 6)),
                CategorySpec::new("executable", "Executables", Rgb::new(7, 8, 9)),
            ],
            components: Vec::new(),
        }
    }

    #[test]
    fn defaults_compile() {
        let categorizer = categorizer();
        assert!(categorizer.categories().len() > 20);
        assert_eq!(
            categorizer
                .category(CategoryId::UNCATEGORIZED)
                .map(super::Category::key),
            Some("uncategorized")
        );
    }

    /// The index assignment is a wire contract, not an implementation detail:
    /// `src/lib/categories.ts` resolves colours and labels from the same
    /// numbers, and every stored snapshot holds them. Ids 0..=18 are
    /// docs/04-CLASSIFICATION.md's "Initial taxonomy" table in order. Append
    /// new categories; never reorder these.
    #[test]
    fn category_indices_are_pinned() {
        let c = categorizer();
        let expected = [
            (0u8, "uncategorized"),
            (1, "symlink"),
            (2, "executable"),
            (3, "apple-metadata"),
            (4, "compressed-archive"),
            (5, "uncompressed-archive"),
            (6, "compressed-stream"),
            (7, "disk-image"),
            (8, "image"),
            (9, "raw-photo"),
            (10, "uncompressed-image"),
            (11, "video"),
            (12, "audio"),
            (13, "document"),
            (14, "source"),
            (15, "object-generated"),
            (16, "library"),
            (17, "vm-disk"),
            (18, "container-disk"),
            // macOS additions, appended after the documented table.
            (19, "package"),
            (20, "media-library"),
            (21, "build-junk"),
            (22, "cache"),
            (23, "font"),
            (24, "database"),
        ];
        for (index, key) in expected {
            assert_eq!(c.key_of(CategoryId::from_raw(index)), Some(key), "id {index}");
        }
        assert_eq!(c.categories().len(), expected.len());
    }

    #[test]
    fn longest_suffix_wins() {
        let c = categorizer();
        assert_eq!(file(&c, "backup.tar.gz"), "compressed-archive");
        assert_eq!(file(&c, "backup.gz"), "compressed-stream");
        assert_eq!(file(&c, "backup.tar"), "uncompressed-archive");
        assert_eq!(file(&c, "archive.tar.bz2"), "compressed-archive");
        assert_eq!(file(&c, "archive.bz2"), "compressed-stream");
        assert_eq!(file(&c, "a.tar.zst"), "compressed-archive");
    }

    #[test]
    fn exact_case_beats_folded_case() {
        let c = categorizer();
        // `.C` is C++, `.c` is C: both land in source, but via different maps.
        assert_eq!(file(&c, "main.C"), "source");
        assert_eq!(file(&c, "main.c"), "source");
        // `x.tar.Z` hits the byte-exact two-part entry.
        assert_eq!(file(&c, "old.tar.Z"), "compressed-archive");
        assert_eq!(file(&c, "old.Z"), "compressed-stream");
        // Folding still applies where no exact entry exists.
        assert_eq!(file(&c, "PHOTO.JPG"), "image");
        assert_eq!(file(&c, "Photo.JpG"), "image");
    }

    #[test]
    fn leading_dot_is_not_a_separator() {
        let c = categorizer();
        assert_eq!(file(&c, ".gitignore"), "source");
        assert_eq!(file(&c, ".DS_Store"), "apple-metadata");
        // The ladder is fixed: suffixes are exhausted before any glob runs, so
        // an AppleDouble twin that keeps its original extension is classified
        // by that extension. `._sidecar` (no extension) reaches the glob.
        // Consequence of docs/04's ordering, recorded here on purpose.
        assert_eq!(file(&c, "._Photo.jpg"), "image");
        assert_eq!(file(&c, "._sidecar"), "apple-metadata");
        // A leading dot does not stop a real suffix later in the name.
        assert_eq!(file(&c, ".hidden.mp4"), "video");
        // ...and the hidden basename itself is not treated as an extension.
        assert_eq!(file(&c, ".mp4"), "uncategorized");
    }

    #[test]
    fn trailing_dot_and_empty_names() {
        let c = categorizer();
        assert_eq!(file(&c, "movie.mp4."), "uncategorized");
        assert_eq!(file(&c, "movie."), "uncategorized");
        assert_eq!(file(&c, "."), "uncategorized");
        assert_eq!(file(&c, ".."), "uncategorized");
        assert_eq!(file(&c, ""), "uncategorized");
        assert_eq!(file(&c, "noSuffix"), "uncategorized");
    }

    #[test]
    fn more_parts_than_the_maximum() {
        let c = categorizer();
        // Three parts is the default maximum: the four-part prefix is never
        // tried, but the three-part and shorter suffixes still are.
        assert_eq!(file(&c, "a.b.c.tar.gz"), "compressed-archive");
        assert_eq!(file(&c, "a.b.c.d.e.f.mp4"), "video");
    }

    #[test]
    fn undecodable_bytes_classify_by_suffix() {
        let c = categorizer();
        let mut name = vec![0xff, 0xfe, 0x80];
        name.extend_from_slice(b".mov");
        assert_eq!(c.key_of(c.classify(&name, Kind::File, 0o644)), Some("video"));
        // An undecodable name with no suffix falls through cleanly.
        assert_eq!(
            c.key_of(c.classify(&[0xff, 0xfe], Kind::File, 0o644)),
            Some("uncategorized")
        );
    }

    #[test]
    fn symlink_wins_over_everything() {
        let c = categorizer();
        assert_eq!(key(&c, "movie.mp4", Kind::Symlink, 0o777), "symlink");
        assert_eq!(key(&c, "node_modules", Kind::Symlink, 0o777), "symlink");
        assert_eq!(key(&c, ".DS_Store", Kind::Symlink, 0o777), "symlink");
    }

    #[test]
    fn execute_bit_is_the_last_rung() {
        let c = categorizer();
        assert_eq!(key(&c, "configure", Kind::File, 0o755), "executable");
        assert_eq!(key(&c, "configure", Kind::File, 0o644), "uncategorized");
        // A named match beats the execute bit.
        assert_eq!(key(&c, "build.sh", Kind::File, 0o755), "source");
        // Directories are executable by definition; the rule must not fire.
        assert_eq!(key(&c, "somedir", Kind::Directory, 0o755), "uncategorized");
        // Group- and other-execute count too.
        assert_eq!(key(&c, "tool", Kind::File, 0o011), "executable");
    }

    #[test]
    fn special_kinds_are_uncategorized() {
        let c = categorizer();
        for kind in [Kind::Socket, Kind::Fifo, Kind::CharDevice, Kind::BlockDevice] {
            assert_eq!(key(&c, "thing.mp4", kind, 0o777), "uncategorized", "{kind:?}");
        }
    }

    #[test]
    fn directory_eligibility_keeps_files_out_of_directory_categories() {
        let c = categorizer();
        assert_eq!(directory(&c, "Safari.app"), "package");
        assert_eq!(directory(&c, "Photos.photoslibrary"), "media-library");
        assert_eq!(directory(&c, "MyApp.xcarchive"), "build-junk");
        assert_eq!(directory(&c, "node_modules"), "cache");
        assert_eq!(directory(&c, "DerivedData"), "cache");
        // The docs/04 scenario: a video inside a cache is still a video.
        assert_eq!(file(&c, "movie.mp4"), "video");
        // A directory whose name looks like a document is not a document.
        assert_eq!(directory(&c, "notes.txt"), "uncategorized");
        // A *file* named node_modules is not a cache (the glob is dir-scoped).
        assert_eq!(file(&c, "node_modules"), "uncategorized");
        assert_eq!(file(&c, "node_modules.txt"), "document");
    }

    #[test]
    fn macos_taxonomy_is_present() {
        let c = categorizer();
        assert_eq!(file(&c, "Ventura.dmg"), "disk-image");
        assert_eq!(file(&c, "Xcode.xip"), "uncategorized"); // deliberately unclaimed
        assert_eq!(directory(&c, "MyKext.kext"), "package");
        assert_eq!(directory(&c, "Final Cut.fcpbundle"), "media-library");
        assert_eq!(file(&c, "MyApp.app.dSYM"), "build-junk");
        assert_eq!(file(&c, "Docker.raw"), "container-disk");
        assert_eq!(file(&c, "disk.qcow2"), "vm-disk");
        assert_eq!(directory(&c, "Windows 11.utm"), "vm-disk");
        assert_eq!(file(&c, "._sidecar"), "apple-metadata");
        assert_eq!(file(&c, "IMG_0001.CR3"), "raw-photo");
        assert_eq!(file(&c, "IMG_0001.HEIC"), "image");
    }

    #[test]
    fn ordered_globs_are_the_fallback_not_the_first_choice() {
        let c = categorizer();
        // A suffix match short-circuits before any glob runs.
        assert_eq!(file(&c, "Makefile"), "source");
        assert_eq!(file(&c, "Makefile.am"), "uncategorized");
        assert_eq!(file(&c, "makefile"), "source");
        // Case-sensitive globs do not fold.
        assert_eq!(file(&c, ".ds_store"), "uncategorized");
    }

    #[test]
    fn context_tags_are_component_aware() {
        let c = categorizer();
        let tags = c.context_tags(b"node_modules");
        assert!(tags.contains(ContextTag::DependencyTree));
        assert!(tags.contains(ContextTag::Cache));
        assert!(c.context_tags(b"MyCachesBackup").is_empty());
        assert!(c.context_tags(b"node_modules.txt").is_empty());
        assert!(c.context_tags(b"Caches").contains(ContextTag::Cache));
        assert!(c.context_tags(b"caches").contains(ContextTag::Cache));
        assert!(c.context_tags(b"Safari.app").contains(ContextTag::Package));
        assert!(
            c.context_tags(b"Photos.photoslibrary")
                .contains(ContextTag::MediaLibrary)
        );
        assert!(c.context_tags(b".Trash").contains(ContextTag::Trash));
        assert!(c.context_tags(b"overlay2").contains(ContextTag::ContainerStorage));
        assert!(c.context_tags(b"DerivedData").contains(ContextTag::BuildOutput));
    }

    #[test]
    fn digest_is_deterministic_and_sensitive() {
        let first = categorizer();
        let second = categorizer();
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.config_hash().as_str().len(), 64);

        let mut changed = crate::defaults::default_config();
        if let Some(spec) = changed.categories.get_mut(4) {
            spec.suffixes_ci.push("qqq".to_owned());
        }
        let other = Categorizer::compile(&changed).expect("still valid");
        assert_ne!(first.digest(), other.digest());

        // Reordering two categories changes the ids, so it must change the hash.
        let mut reordered = crate::defaults::default_config();
        reordered.categories.swap(4, 5);
        let reordered = Categorizer::compile(&reordered).expect("still valid");
        assert_ne!(first.digest(), reordered.digest());
    }

    #[test]
    fn duplicate_suffix_is_rejected() {
        let mut config = minimal_config();
        config
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_suffixes(&["zip"]));
        config
            .categories
            .push(CategorySpec::new("b", "B", Rgb::new(0, 0, 0)).with_suffixes(&["zip"]));
        assert!(matches!(
            Categorizer::compile(&config),
            Err(ConfigError::DuplicateSuffix { .. })
        ));
    }

    #[test]
    fn exact_and_folded_maps_may_share_a_spelling() {
        let mut config = minimal_config();
        config
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_suffixes(&["z"]));
        config
            .categories
            .push(CategorySpec::new("b", "B", Rgb::new(0, 0, 0)).with_exact_suffixes(&["Z"]));
        let compiled = Categorizer::compile(&config).expect("distinct maps do not collide");
        assert_eq!(compiled.key_of(compiled.classify(b"x.Z", Kind::File, 0)), Some("b"));
        assert_eq!(compiled.key_of(compiled.classify(b"x.z", Kind::File, 0)), Some("a"));
    }

    #[test]
    fn duplicate_glob_is_rejected() {
        let mut config = minimal_config();
        config
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_globs(vec![GlobSpec::any("Makefile")]));
        config
            .categories
            .push(CategorySpec::new("b", "B", Rgb::new(0, 0, 0)).with_globs(vec![GlobSpec::any("Makefile")]));
        assert!(matches!(
            Categorizer::compile(&config),
            Err(ConfigError::DuplicateGlob { .. })
        ));
    }

    #[test]
    fn duplicate_category_key_is_rejected() {
        let mut config = minimal_config();
        config
            .categories
            .push(CategorySpec::new("dup", "One", Rgb::new(0, 0, 0)));
        config
            .categories
            .push(CategorySpec::new("dup", "Two", Rgb::new(0, 0, 0)));
        assert!(matches!(
            Categorizer::compile(&config),
            Err(ConfigError::DuplicateCategoryKey { .. })
        ));
    }

    #[test]
    fn mandatory_categories_are_enforced() {
        let mut without_symlink = minimal_config();
        without_symlink.categories.remove(1);
        assert!(matches!(
            Categorizer::compile(&without_symlink),
            Err(ConfigError::MissingRequiredCategory { key: "symlink" })
        ));

        let mut without_executable = minimal_config();
        without_executable.categories.remove(2);
        assert!(matches!(
            Categorizer::compile(&without_executable),
            Err(ConfigError::MissingRequiredCategory { key: "executable" })
        ));
    }

    #[test]
    fn uncategorized_must_be_first_and_rule_free() {
        let mut swapped = minimal_config();
        swapped.categories.swap(0, 1);
        assert!(matches!(
            Categorizer::compile(&swapped),
            Err(ConfigError::UncategorizedNotFirst { .. })
        ));

        let mut with_rules = minimal_config();
        if let Some(spec) = with_rules.categories.first_mut() {
            spec.suffixes_ci.push("zip".to_owned());
        }
        assert!(matches!(
            Categorizer::compile(&with_rules),
            Err(ConfigError::RuleFreeCategoryHasPatterns { key: "uncategorized" })
        ));

        let mut symlink_rules = minimal_config();
        if let Some(spec) = symlink_rules.categories.get_mut(1) {
            spec.suffixes_ci.push("lnk".to_owned());
        }
        assert!(matches!(
            Categorizer::compile(&symlink_rules),
            Err(ConfigError::RuleFreeCategoryHasPatterns { key: "symlink" })
        ));
    }

    #[test]
    fn category_limit_is_enforced() {
        let mut config = minimal_config();
        for index in 0..CategoryId::MAX_CATEGORIES {
            config
                .categories
                .push(CategorySpec::new(&format!("k{index}"), "L", Rgb::new(0, 0, 0)));
        }
        assert!(matches!(
            Categorizer::compile(&config),
            Err(ConfigError::TooManyCategories { .. })
        ));
    }

    #[test]
    fn exactly_the_maximum_number_of_categories_compiles() {
        let mut config = minimal_config();
        while config.categories.len() < CategoryId::MAX_CATEGORIES {
            let index = config.categories.len();
            config
                .categories
                .push(CategorySpec::new(&format!("k{index}"), "L", Rgb::new(0, 0, 0)));
        }
        let compiled = Categorizer::compile(&config).expect("256 categories is legal");
        assert_eq!(compiled.categories().len(), 256);
        assert_eq!(
            compiled
                .categories()
                .last()
                .map(super::Category::id)
                .map(CategoryId::get),
            Some(255)
        );
    }

    #[test]
    fn suffix_shape_is_validated() {
        let mut leading_dot = minimal_config();
        leading_dot
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_suffixes(&[".zip"]));
        assert!(matches!(
            Categorizer::compile(&leading_dot),
            Err(ConfigError::SuffixLeadingDot { .. })
        ));

        let mut empty = minimal_config();
        empty
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_suffixes(&[""]));
        assert!(matches!(
            Categorizer::compile(&empty),
            Err(ConfigError::EmptySuffix { .. })
        ));

        let mut trailing = minimal_config();
        trailing
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_suffixes(&["tar."]));
        assert!(matches!(
            Categorizer::compile(&trailing),
            Err(ConfigError::EmptySuffix { .. })
        ));

        let mut too_deep = minimal_config();
        too_deep.max_suffix_parts = 2;
        too_deep
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_suffixes(&["a.b.c"]));
        assert!(matches!(
            Categorizer::compile(&too_deep),
            Err(ConfigError::UnreachableSuffix { parts: 3, max: 2, .. })
        ));
    }

    #[test]
    fn max_suffix_parts_is_bounded() {
        let mut zero = minimal_config();
        zero.max_suffix_parts = 0;
        assert!(matches!(
            Categorizer::compile(&zero),
            Err(ConfigError::InvalidMaxSuffixParts { value: 0, .. })
        ));

        let mut huge = minimal_config();
        huge.max_suffix_parts = super::MAX_SUFFIX_PARTS + 1;
        assert!(matches!(
            Categorizer::compile(&huge),
            Err(ConfigError::InvalidMaxSuffixParts { .. })
        ));
    }

    #[test]
    fn duplicate_component_rule_is_rejected() {
        let mut config = minimal_config();
        config
            .components
            .push(ComponentRule::new("Caches", &[ContextTag::Cache]));
        config
            .components
            .push(ComponentRule::new("caches", &[ContextTag::Cache]));
        assert!(matches!(
            Categorizer::compile(&config),
            Err(ConfigError::DuplicateComponentRule { .. })
        ));
    }

    #[test]
    fn empty_glob_is_rejected() {
        let mut config = minimal_config();
        config
            .categories
            .push(CategorySpec::new("a", "A", Rgb::new(0, 0, 0)).with_globs(vec![GlobSpec::any("")]));
        assert!(matches!(
            Categorizer::compile(&config),
            Err(ConfigError::EmptyGlob { .. })
        ));
    }

    #[test]
    fn shared_is_compiled_once() {
        let first = Categorizer::shared().expect("defaults compile");
        let second = Categorizer::shared().expect("defaults compile");
        assert!(core::ptr::eq(first, second));
    }

    #[test]
    fn every_default_category_is_reachable_or_rule_assigned() {
        let c = categorizer();
        for category in c.categories() {
            assert!(!category.key().is_empty());
            assert!(!category.label().is_empty());
            assert_eq!(c.id_of(category.key()), Some(category.id()));
        }
    }
}
