//! Turning CLI arguments into [`ScanOptions`].
//!
//! [`ScanOptions`] is `#[non_exhaustive]`, so it is built by mutating a
//! `Default` rather than with a struct literal. That is the point of the
//! attribute: when the scan crate adds an option, this file stops compiling
//! only if the new option needs a CLI decision, and every other caller keeps
//! the documented default.

use rdirstat_core::ScanOptions;

use crate::cli::{ScanArgs, VerifyArgs};
use crate::exclude;

/// Options for `rdirstat scan`.
#[allow(
    clippy::field_reassign_with_default,
    reason = "ScanOptions is #[non_exhaustive]; a struct literal is not permitted outside rdirstat-core"
)]
pub(crate) fn for_scan(args: &ScanArgs, root_is_slash: bool) -> ScanOptions {
    let mut options = ScanOptions::default();
    options.cross_filesystems = args.cross_filesystems;
    options.count_hard_links_once = !args.count_hard_links_every_time;
    options.apply_default_exclusions = !args.no_default_exclusions;
    options.aggregate_below_bytes = args.aggregate_below;
    options.workers = args.threads;
    options.memory_limit_bytes = args.memory_limit;
    options.exclusions = exclude::effective_rules(&args.exclude, options.apply_default_exclusions, root_is_slash);
    options
}

/// Options for `rdirstat verify`.
///
/// Deliberately different from [`for_scan`]: exclusions are off and hard-link
/// policy is on, because those are the two things that make a `du` comparison
/// mean anything.
#[allow(
    clippy::field_reassign_with_default,
    reason = "ScanOptions is #[non_exhaustive]; a struct literal is not permitted outside rdirstat-core"
)]
pub(crate) fn for_verify(args: &VerifyArgs) -> ScanOptions {
    let mut options = ScanOptions::default();
    options.cross_filesystems = args.cross_filesystems;
    options.count_hard_links_once = true;
    options.apply_default_exclusions = false;
    options.aggregate_below_bytes = None;
    options.workers = args.threads;
    options.memory_limit_bytes = None;
    options.exclusions = Vec::new();
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Format, ProgressWhen, Quantity};

    fn scan_args() -> ScanArgs {
        ScanArgs {
            path: "/tmp".into(),
            quantity: Quantity::Allocated,
            format: Format::Text,
            stats: false,
            top_down: false,
            exclude: vec!["node_modules".to_owned()],
            no_default_exclusions: false,
            cross_filesystems: false,
            threads: Some(4),
            aggregate_below: Some(4096),
            count_hard_links_every_time: false,
            memory_limit: None,
            max_depth: 3,
            max_children: 16,
            progress: ProgressWhen::Never,
            trace_chrome: None,
        }
    }

    #[test]
    fn scan_options_carry_every_flag_through() {
        let options = for_scan(&scan_args(), false);
        assert!(!options.cross_filesystems);
        assert!(options.count_hard_links_once);
        assert!(options.apply_default_exclusions);
        assert_eq!(options.aggregate_below_bytes, Some(4096));
        assert_eq!(options.workers, Some(4));
        // one user rule plus the shipped defaults
        assert_eq!(options.exclusions.len(), 1 + exclude::DEFAULT_EXCLUSIONS.len());
    }

    #[test]
    fn disabling_the_defaults_leaves_only_the_user_rules() {
        let mut args = scan_args();
        args.no_default_exclusions = true;
        let options = for_scan(&args, true);
        assert!(!options.apply_default_exclusions);
        assert_eq!(options.exclusions.len(), 1);
    }

    #[test]
    fn a_root_scan_adds_the_volumes_rule() {
        let options = for_scan(&scan_args(), true);
        assert_eq!(options.exclusions.len(), 2 + exclude::DEFAULT_EXCLUSIONS.len());
    }

    #[test]
    fn verify_options_disable_exclusions_and_keep_hard_link_policy() {
        let args = VerifyArgs {
            path: "/tmp".into(),
            format: Format::Text,
            threads: Some(2),
            cross_filesystems: false,
            tolerance_bytes: 0,
            progress: ProgressWhen::Never,
        };
        let options = for_verify(&args);
        assert!(!options.apply_default_exclusions);
        assert!(options.exclusions.is_empty());
        assert!(options.count_hard_links_once);
        assert_eq!(options.workers, Some(2));
        assert!(options.aggregate_below_bytes.is_none());
    }
}
