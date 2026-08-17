//! Exclusion rules: compiled once, ordered, first match wins.
//!
//! Patterns are matched against **bytes**, never against a lossy `String`
//! rendering of a path: macOS names are NFD byte sequences and are not
//! guaranteed to be UTF-8. `docs/02-SCANNER.md` requires that nothing compiles
//! a pattern inside the scan loop, so every rule is turned into a
//! [`Compiled`] segment list up front and rejected there if it is malformed.

use rdirstat_core::{ExclusionRule, RuleAction, RuleScope, RuleSyntax};

/// The shipped macOS defaults, as root-relative path patterns.
///
/// `docs/02-SCANNER.md` writes these as absolute paths; a scan of `/` sees them
/// as these root-relative ones. They are conservative on purpose: `.Trash` is
/// *not* excluded, because it is frequently the answer to "where did my disk
/// go", and the firmlink entries are defense in depth for readers that do not
/// surface `SF_FIRMLINK`.
pub(crate) const DEFAULT_EXCLUSIONS: [&str; 7] = [
    "System/Volumes/Data",
    "System/Volumes/VM",
    "System/Volumes/Preboot",
    ".Spotlight-V100",
    ".fseventsd",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
];

/// The extra default that only applies when the scan root is `/`.
pub(crate) const ROOT_ONLY_EXCLUSION: &str = "Volumes/*";

/// A rule that has been validated and split into path segments.
#[derive(Debug)]
struct Compiled {
    action: RuleAction,
    scope: RuleScope,
    case_sensitive: bool,
    /// Pattern segments, split on `/`. A `**` segment matches any number of
    /// path segments including zero.
    segments: Vec<Vec<u8>>,
}

/// Why a rule could not be compiled.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RuleError {
    /// Regular expressions are a documented rule syntax, but this build has no
    /// regex engine linked, and silently treating one as a glob would exclude
    /// the wrong subtree.
    #[error("regex exclusion `{pattern}` is not supported by this build; use a glob")]
    RegexUnsupported {
        /// The offending pattern.
        pattern: String,
    },
    /// An empty pattern matches nothing useful and is almost always a quoting
    /// accident.
    #[error("empty exclusion pattern")]
    Empty,
}

/// The compiled, ordered rule set.
#[derive(Debug, Default)]
pub(crate) struct Rules {
    rules: Vec<Compiled>,
}

impl Rules {
    /// Compiles `rules` in order.
    ///
    /// # Errors
    ///
    /// [`RuleError`] if a rule is empty or uses an unsupported syntax. This
    /// happens at configuration load, never inside the scan loop.
    pub(crate) fn compile(rules: &[ExclusionRule]) -> Result<Self, RuleError> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            if rule.syntax != RuleSyntax::Glob {
                return Err(RuleError::RegexUnsupported {
                    pattern: rule.pattern.clone(),
                });
            }
            if rule.pattern.is_empty() {
                return Err(RuleError::Empty);
            }
            let segments = rule
                .pattern
                .trim_start_matches('/')
                .split('/')
                .map(|segment| {
                    if rule.case_sensitive {
                        segment.as_bytes().to_vec()
                    } else {
                        segment.as_bytes().to_ascii_lowercase()
                    }
                })
                .collect();
            compiled.push(Compiled {
                action: rule.action,
                scope: rule.scope,
                case_sensitive: rule.case_sensitive,
                segments,
            });
        }
        Ok(Self { rules: compiled })
    }

    /// Whether the rule set is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `name` (an entry name) at `relative` (its root-relative path)
    /// is excluded. First match wins; an unmatched entry is included.
    pub(crate) fn is_excluded(&self, name: &[u8], relative: &[u8]) -> bool {
        for rule in &self.rules {
            let subject = match rule.scope {
                RuleScope::DirectoryName => name,
                // RootRelativePath, and any scope a later version of core adds:
                // matching the whole relative path is the conservative reading.
                _ => relative,
            };
            let matched = if rule.case_sensitive {
                match_segments(&rule.segments, subject)
            } else {
                let lowered = subject.to_ascii_lowercase();
                match_segments(&rule.segments, &lowered)
            };
            if matched {
                return matches!(rule.action, RuleAction::Exclude);
            }
        }
        false
    }
}

/// Builds the effective rule list: user rules first, then the shipped defaults.
///
/// User rules come first because first-match-wins makes ordering the whole
/// semantics: `--exclude` must be able to say something the defaults would
/// otherwise decide.
pub(crate) fn effective_rules(user_globs: &[String], apply_defaults: bool, root_is_slash: bool) -> Vec<ExclusionRule> {
    let mut rules: Vec<ExclusionRule> = user_globs
        .iter()
        .map(|pattern| ExclusionRule {
            action: RuleAction::Exclude,
            scope: if pattern.contains('/') {
                RuleScope::RootRelativePath
            } else {
                RuleScope::DirectoryName
            },
            syntax: RuleSyntax::Glob,
            pattern: pattern.clone(),
            case_sensitive: false,
        })
        .collect();

    if apply_defaults {
        for pattern in DEFAULT_EXCLUSIONS {
            rules.push(ExclusionRule {
                action: RuleAction::Exclude,
                scope: RuleScope::RootRelativePath,
                syntax: RuleSyntax::Glob,
                pattern: pattern.to_owned(),
                case_sensitive: false,
            });
        }
        if root_is_slash {
            rules.push(ExclusionRule {
                action: RuleAction::Exclude,
                scope: RuleScope::RootRelativePath,
                syntax: RuleSyntax::Glob,
                pattern: ROOT_ONLY_EXCLUSION.to_owned(),
                case_sensitive: false,
            });
        }
    }
    rules
}

/// Matches a segmented pattern against a `/`-separated subject.
fn match_segments(pattern: &[Vec<u8>], subject: &[u8]) -> bool {
    let parts: Vec<&[u8]> = subject.split(|byte| *byte == b'/').collect();
    match_from(pattern, &parts)
}

/// Iterative-with-backtracking segment match, `**` aware.
fn match_from(pattern: &[Vec<u8>], parts: &[&[u8]]) -> bool {
    let mut pattern_index = 0usize;
    let mut part_index = 0usize;
    // The most recent `**` position, so a failed match can backtrack to it
    // instead of recursing. Depth is unbounded input; recursion is not an
    // option here for the same reason it is not one in the rollup.
    let mut star_pattern: Option<usize> = None;
    let mut star_part = 0usize;

    while part_index < parts.len() {
        if pattern_index < pattern.len() {
            if pattern[pattern_index] == b"**" {
                star_pattern = Some(pattern_index);
                star_part = part_index;
                pattern_index += 1;
                continue;
            }
            if match_one(&pattern[pattern_index], parts[part_index]) {
                pattern_index += 1;
                part_index += 1;
                continue;
            }
        }
        if let Some(star) = star_pattern {
            star_part += 1;
            part_index = star_part;
            pattern_index = star + 1;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b"**" {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// Matches one pattern segment (`*` and `?`, neither crossing `/`) against one
/// path segment.
fn match_one(pattern: &[u8], text: &[u8]) -> bool {
    let mut pattern_index = 0usize;
    let mut text_index = 0usize;
    let mut star: Option<usize> = None;
    let mut star_text = 0usize;

    while text_index < text.len() {
        match pattern.get(pattern_index) {
            Some(b'*') => {
                star = Some(pattern_index);
                star_text = text_index;
                pattern_index += 1;
            }
            Some(b'?') => {
                pattern_index += 1;
                text_index += 1;
            }
            Some(byte) if *byte == text[text_index] => {
                pattern_index += 1;
                text_index += 1;
            }
            _ => {
                if let Some(position) = star {
                    star_text += 1;
                    text_index = star_text;
                    pattern_index = position + 1;
                } else {
                    return false;
                }
            }
        }
    }
    while pattern.get(pattern_index) == Some(&b'*') {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(patterns: &[&str]) -> Rules {
        let owned: Vec<String> = patterns.iter().map(|pattern| (*pattern).to_owned()).collect();
        let effective = effective_rules(&owned, false, false);
        Rules::compile(&effective).expect("test patterns compile")
    }

    #[test]
    fn a_bare_name_matches_the_entry_name_at_any_depth() {
        let compiled = rules(&["node_modules"]);
        assert!(compiled.is_excluded(b"node_modules", b"a/b/node_modules"));
        assert!(!compiled.is_excluded(b"src", b"a/b/src"));
    }

    #[test]
    fn a_path_pattern_is_anchored_at_the_scan_root() {
        let compiled = rules(&["build/artifacts"]);
        assert!(compiled.is_excluded(b"artifacts", b"build/artifacts"));
        assert!(!compiled.is_excluded(b"artifacts", b"deep/build/artifacts"));
    }

    #[test]
    fn double_star_crosses_separators_and_single_star_does_not() {
        let compiled = rules(&["**/target"]);
        assert!(compiled.is_excluded(b"target", b"a/b/c/target"));
        assert!(compiled.is_excluded(b"target", b"target"));

        let shallow = rules(&["a/*/target"]);
        assert!(shallow.is_excluded(b"target", b"a/b/target"));
        assert!(!shallow.is_excluded(b"target", b"a/b/c/target"));
    }

    #[test]
    fn matching_is_case_insensitive_by_default_like_the_volume() {
        let compiled = rules(&["Caches"]);
        assert!(compiled.is_excluded(b"caches", b"Library/caches"));
    }

    #[test]
    fn non_utf8_names_still_match_byte_patterns() {
        let compiled = rules(&["*.bin"]);
        assert!(compiled.is_excluded(b"\xff\xfe.bin", b"dir/\xff\xfe.bin"));
    }

    #[test]
    fn the_shipped_defaults_hide_the_firmlink_and_index_directories() {
        let effective = effective_rules(&[], true, true);
        let compiled = Rules::compile(&effective).expect("defaults compile");
        assert!(compiled.is_excluded(b"Data", b"System/Volumes/Data"));
        assert!(compiled.is_excluded(b".fseventsd", b".fseventsd"));
        assert!(compiled.is_excluded(b"tuf8tb", b"Volumes/tuf8tb"));
        // Trash is deliberately included: it is often the missing space.
        assert!(!compiled.is_excluded(b".Trash", b"Users/x/.Trash"));
    }

    #[test]
    fn a_regex_rule_is_rejected_at_load_not_silently_treated_as_a_glob() {
        let rule = ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::RootRelativePath,
            syntax: RuleSyntax::Regex,
            pattern: "^build$".to_owned(),
            case_sensitive: true,
        };
        let error = Rules::compile(&[rule]).expect_err("regex must be rejected");
        assert!(matches!(error, RuleError::RegexUnsupported { .. }));
    }
}
