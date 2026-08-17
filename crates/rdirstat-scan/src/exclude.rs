//! Exclusions: compiled once, ordered, first match wins.
//!
//! Rules are evaluated *before* a directory is opened. Nothing compiles inside
//! the scan loop and nothing allocates per match beyond an inline
//! [`SmallVec`](smallvec::SmallVec) of path segments
//! (docs/02-SCANNER.md#exclusions).
//!
//! `RuleSyntax::Regex` is **rejected at compile time in this build**: the
//! workspace pins no regex engine, and a rule that silently never matches would
//! be worse than a refusal. `RuleSyntax::Glob` covers every shipped default.

use std::path::Path;

use rdirstat_core::{ExclusionRule, RuleAction, RuleScope, RuleSyntax, StartError};
use smallvec::SmallVec;

/// What a compiled rule set says about one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No rule matched, or the first match was an include.
    Keep,
    /// The first matching rule was an exclude. The directory is retained as a
    /// flagged marker and never opened.
    Skip,
}

#[derive(Clone, Debug)]
struct CompiledRule {
    action: RuleAction,
    scope: RuleScope,
    pattern: Vec<u8>,
    case_sensitive: bool,
}

/// An ordered, compiled rule set.
#[derive(Clone, Debug, Default)]
pub struct ExclusionSet {
    rules: Vec<CompiledRule>,
    source: Vec<ExclusionRule>,
}

impl ExclusionSet {
    /// Compiles `rules` in order.
    ///
    /// # Errors
    ///
    /// [`StartError::InvalidOptions`] for an unsupported syntax or a pattern
    /// this build refuses (an empty pattern, or a glob with an unterminated
    /// character class). Compiling here is the only place a bad pattern can
    /// surface; the scan loop never sees one.
    pub fn compile(rules: Vec<ExclusionRule>) -> Result<Self, StartError> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in &rules {
            if rule.pattern.is_empty() {
                return Err(StartError::InvalidOptions {
                    detail: "an exclusion pattern may not be empty".to_owned(),
                });
            }
            match rule.syntax {
                RuleSyntax::Glob => {}
                _ => {
                    return Err(StartError::InvalidOptions {
                        detail: format!(
                            "rule {:?} uses {:?}, which this build does not support; use a glob",
                            rule.pattern, rule.syntax
                        ),
                    });
                }
            }
            let pattern = rule.pattern.as_bytes().to_vec();
            validate_glob(&pattern).map_err(|detail| StartError::InvalidOptions { detail })?;
            compiled.push(CompiledRule {
                action: rule.action,
                scope: rule.scope,
                pattern,
                case_sensitive: rule.case_sensitive,
            });
        }
        Ok(Self {
            rules: compiled,
            source: rules,
        })
    }

    /// The rules as configured, for persistence with the result.
    #[must_use]
    pub fn rules(&self) -> &[ExclusionRule] {
        &self.source
    }

    /// Whether the set has no rules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Evaluates `name` (one path component) and `relative` (the path below the
    /// scan root, with no leading separator) against the rules.
    ///
    /// First match wins, so an `Include` rule placed before an `Exclude` rule
    /// wins for the paths it covers.
    #[must_use]
    pub fn verdict(&self, name: &[u8], relative: &[u8]) -> Verdict {
        for rule in &self.rules {
            let matched = match rule.scope {
                RuleScope::DirectoryName => match_segment(&rule.pattern, name, rule.case_sensitive),
                RuleScope::RootRelativePath => {
                    !relative.is_empty() && match_path(&rule.pattern, relative, rule.case_sensitive)
                }
                _ => false,
            };
            if matched {
                return match rule.action {
                    RuleAction::Exclude => Verdict::Skip,
                    _ => Verdict::Keep,
                };
            }
        }
        Verdict::Keep
    }

    /// A canonical, stable rendering of the rule set, for the config digest.
    #[must_use]
    pub fn canonical_text(&self) -> String {
        let mut out = String::new();
        for rule in &self.source {
            out.push_str(match rule.action {
                RuleAction::Exclude => "exclude",
                _ => "include",
            });
            out.push('\t');
            out.push_str(match rule.scope {
                RuleScope::DirectoryName => "name",
                _ => "path",
            });
            out.push('\t');
            out.push_str(match rule.syntax {
                RuleSyntax::Regex => "regex",
                _ => "glob",
            });
            out.push('\t');
            out.push_str(if rule.case_sensitive { "cs" } else { "ci" });
            out.push('\t');
            out.push_str(&rule.pattern);
            out.push('\n');
        }
        out
    }
}

/// A glob rule on the root-relative path.
#[must_use]
pub fn path_rule(pattern: &str) -> ExclusionRule {
    ExclusionRule {
        action: RuleAction::Exclude,
        scope: RuleScope::RootRelativePath,
        syntax: RuleSyntax::Glob,
        pattern: pattern.to_owned(),
        case_sensitive: true,
    }
}

/// The shipped conservative defaults, from docs/02-SCANNER.md#exclusions.
///
/// Patterns are root-relative, so the same list works for a volume scan and for
/// a subtree scan. The firmlink and `/Volumes/*` entries only make sense when
/// the root *is* `/`, and are omitted otherwise — excluding a literal
/// `System/Volumes/Data` under some unrelated root would be a surprise.
///
/// `.Trash` is deliberately **not** excluded: it often explains missing space.
/// A user who wants it gone adds `**/.Trash` themselves.
#[must_use]
pub fn default_exclusions(root: &Path) -> Vec<ExclusionRule> {
    let mut rules = Vec::with_capacity(8);
    if root == Path::new("/") {
        // Firmlink defense in depth: the data volume is already the root's
        // storage, and descending here walks it a second time.
        rules.push(path_rule("System/Volumes/Data"));
        // Volatile swap; reported separately rather than counted as user data.
        rules.push(path_rule("System/Volumes/VM"));
        rules.push(path_rule("System/Volumes/Preboot"));
        // Other mounted volumes are their own scans.
        rules.push(path_rule("Volumes/*"));
    }
    // Volume-root metadata stores. These exist at the root of every volume, so
    // they are root-relative for any root that is a volume.
    rules.push(path_rule(".Spotlight-V100"));
    rules.push(path_rule(".fseventsd"));
    rules.push(path_rule(".DocumentRevisions-V100"));
    rules.push(path_rule(".TemporaryItems"));
    rules
}

/// Rejects a glob this matcher cannot represent.
fn validate_glob(pattern: &[u8]) -> Result<(), String> {
    let mut index = 0;
    while index < pattern.len() {
        if pattern[index] == b'[' {
            let mut cursor = index + 1;
            if matches!(pattern.get(cursor), Some(b'!' | b'^')) {
                cursor += 1;
            }
            let mut closed = false;
            while cursor < pattern.len() {
                if pattern[cursor] == b']' {
                    closed = true;
                    break;
                }
                cursor += 1;
            }
            if !closed {
                return Err(format!(
                    "glob {:?} has an unterminated character class",
                    String::from_utf8_lossy(pattern)
                ));
            }
            index = cursor + 1;
            continue;
        }
        index += 1;
    }
    Ok(())
}

const fn fold(byte: u8, case_sensitive: bool) -> u8 {
    if case_sensitive {
        byte
    } else {
        byte.to_ascii_lowercase()
    }
}

/// Matches one path component. `*` and `?` do not cross a separator because a
/// component contains none.
///
/// Iterative with one backtrack point, so a pathological pattern costs time,
/// never stack.
#[must_use]
pub fn match_segment(pattern: &[u8], text: &[u8], case_sensitive: bool) -> bool {
    let (mut p, mut t) = (0_usize, 0_usize);
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        let advanced = if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    star = Some((p, t));
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => match match_class(pattern, p, text[t], case_sensitive) {
                    Some((next, true)) => {
                        p = next;
                        t += 1;
                        continue;
                    }
                    Some((_, false)) => false,
                    None => fold(b'[', case_sensitive) == fold(text[t], case_sensitive),
                },
                literal => fold(literal, case_sensitive) == fold(text[t], case_sensitive),
            }
        } else {
            false
        };

        if advanced {
            p += 1;
            t += 1;
            continue;
        }
        match star {
            Some((star_p, star_t)) => {
                star = Some((star_p, star_t + 1));
                t = star_t + 1;
                p = star_p + 1;
            }
            None => return false,
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Matches a bracket expression starting at `start`.
///
/// Returns the index just past the closing `]` and whether `ch` matched, or
/// `None` if the class is unterminated (in which case `[` is a literal).
fn match_class(pattern: &[u8], start: usize, ch: u8, case_sensitive: bool) -> Option<(usize, bool)> {
    let mut index = start + 1;
    let negated = matches!(pattern.get(index), Some(b'!' | b'^'));
    if negated {
        index += 1;
    }
    let target = fold(ch, case_sensitive);
    let mut matched = false;
    let mut closed = None;
    while index < pattern.len() {
        if pattern[index] == b']' {
            closed = Some(index + 1);
            break;
        }
        let low = pattern[index];
        if index + 2 < pattern.len() && pattern[index + 1] == b'-' && pattern[index + 2] != b']' {
            let high = pattern[index + 2];
            let (low, high) = (fold(low, case_sensitive), fold(high, case_sensitive));
            if low <= target && target <= high {
                matched = true;
            }
            index += 3;
            continue;
        }
        if fold(low, case_sensitive) == target {
            matched = true;
        }
        index += 1;
    }
    closed.map(|next| (next, matched != negated))
}

/// Matches a root-relative path. `**` spans separators; `*` does not.
#[must_use]
pub fn match_path(pattern: &[u8], text: &[u8], case_sensitive: bool) -> bool {
    let pats: SmallVec<[&[u8]; 8]> = pattern.split(|&byte| byte == b'/').collect();
    let texts: SmallVec<[&[u8]; 16]> = text.split(|&byte| byte == b'/').collect();

    let (mut i, mut j) = (0_usize, 0_usize);
    let mut star: Option<(usize, usize)> = None;

    while j < texts.len() {
        if i < pats.len() && pats[i] == b"**" {
            star = Some((i, j));
            i += 1;
            continue;
        }
        if i < pats.len() && match_segment(pats[i], texts[j], case_sensitive) {
            i += 1;
            j += 1;
            continue;
        }
        match star {
            Some((star_i, star_j)) => {
                star = Some((star_i, star_j + 1));
                j = star_j + 1;
                i = star_i + 1;
            }
            None => return false,
        }
    }

    while i < pats.len() && pats[i] == b"**" {
        i += 1;
    }
    i == pats.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_wildcard_segments() {
        assert!(match_segment(b"Caches", b"Caches", true));
        assert!(!match_segment(b"Caches", b"MyCachesBackup", true));
        assert!(match_segment(b"*.app", b"Safari.app", true));
        assert!(!match_segment(b"*.app", b"Safari.appx", true));
        assert!(match_segment(b"f?le", b"file", true));
        assert!(match_segment(b"*", b"anything", true));
        assert!(match_segment(b"a*b*c", b"axxbyyc", true));
        assert!(!match_segment(b"a*b*c", b"axxbyy", true));
    }

    #[test]
    fn case_folding_is_per_rule_because_apfs_goes_either_way() {
        assert!(!match_segment(b"caches", b"Caches", true));
        assert!(match_segment(b"caches", b"Caches", false));
    }

    #[test]
    fn character_classes_including_negation() {
        assert!(match_segment(b"[abc]at", b"cat", true));
        assert!(!match_segment(b"[abc]at", b"rat", true));
        assert!(match_segment(b"[!abc]at", b"rat", true));
        assert!(match_segment(b"[a-z]9", b"q9", true));
        assert!(!match_segment(b"[a-z]9", b"Q9", true));
        assert!(
            match_segment(b"[unterminated", b"[unterminated", true),
            "an unterminated class is a literal bracket"
        );
    }

    #[test]
    fn double_star_spans_separators_and_single_star_does_not() {
        assert!(match_path(b"Volumes/*", b"Volumes/Data", true));
        assert!(!match_path(b"Volumes/*", b"Volumes/Data/deeper", true));
        assert!(match_path(b"**/.Trash", b".Trash", true));
        assert!(match_path(b"**/.Trash", b"Users/josh/.Trash", true));
        assert!(!match_path(b"**/.Trash", b"Users/josh/.Trashes", true));
        assert!(match_path(b"System/Volumes/Data", b"System/Volumes/Data", true));
        assert!(!match_path(b"System/Volumes/Data", b"System/Volumes/Datastore", true));
    }

    #[test]
    fn non_utf8_names_match_by_bytes() {
        assert!(match_segment(b"*\xff", &[b'a', 0xff], true));
        assert!(match_path(b"**/\xfe", &[b'a', b'/', 0xfe], true));
    }

    #[test]
    fn first_match_wins_and_include_overrides_a_later_exclude() {
        let include_keep = ExclusionRule {
            action: RuleAction::Include,
            scope: RuleScope::DirectoryName,
            syntax: RuleSyntax::Glob,
            pattern: "Keep".to_owned(),
            case_sensitive: true,
        };
        let exclude_all = ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::DirectoryName,
            syntax: RuleSyntax::Glob,
            pattern: "*".to_owned(),
            case_sensitive: true,
        };
        let set = ExclusionSet::compile(vec![include_keep, exclude_all]).expect("compiles");
        assert_eq!(set.verdict(b"Keep", b"a/Keep"), Verdict::Keep);
        assert_eq!(set.verdict(b"Other", b"a/Other"), Verdict::Skip);
    }

    #[test]
    fn the_macos_defaults_only_claim_system_paths_at_the_real_root() {
        let root = ExclusionSet::compile(default_exclusions(Path::new("/"))).expect("compiles");
        assert_eq!(root.verdict(b"Data", b"System/Volumes/Data"), Verdict::Skip);
        assert_eq!(root.verdict(b"VM", b"System/Volumes/VM"), Verdict::Skip);
        assert_eq!(root.verdict(b"Preboot", b"System/Volumes/Preboot"), Verdict::Skip);
        assert_eq!(root.verdict(b"tuf8tb", b"Volumes/tuf8tb"), Verdict::Skip);
        assert_eq!(root.verdict(b".fseventsd", b".fseventsd"), Verdict::Skip);
        assert_eq!(root.verdict(b".Trash", b"Users/josh/.Trash"), Verdict::Keep);

        let subtree = ExclusionSet::compile(default_exclusions(Path::new("/Users/josh"))).expect("compiles");
        assert_eq!(
            subtree.verdict(b"Data", b"System/Volumes/Data"),
            Verdict::Keep,
            "a literal System/Volumes/Data under some other root is ordinary data"
        );
        assert_eq!(subtree.verdict(b".fseventsd", b".fseventsd"), Verdict::Skip);
    }

    #[test]
    fn a_regex_rule_is_refused_at_compile_time_not_ignored() {
        let rule = ExclusionRule {
            action: RuleAction::Exclude,
            scope: RuleScope::DirectoryName,
            syntax: RuleSyntax::Regex,
            pattern: "^cache$".to_owned(),
            case_sensitive: true,
        };
        let error = ExclusionSet::compile(vec![rule]).expect_err("refused");
        assert!(matches!(error, StartError::InvalidOptions { .. }));
    }

    #[test]
    fn an_unterminated_class_is_refused_at_compile_time() {
        let mut rule = path_rule("bad[a-z");
        rule.case_sensitive = true;
        assert!(ExclusionSet::compile(vec![rule]).is_err());
        assert!(ExclusionSet::compile(vec![path_rule("")]).is_err());
    }

    #[test]
    fn the_canonical_text_is_stable_and_ordered() {
        let set = ExclusionSet::compile(default_exclusions(Path::new("/"))).expect("compiles");
        let text = set.canonical_text();
        assert!(
            text.starts_with("exclude\tpath\tglob\tcs\tSystem/Volumes/Data\n"),
            "{text}"
        );
        assert_eq!(text.lines().count(), 8);
    }
}
