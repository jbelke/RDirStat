//! Asking GitHub whether a newer release exists.
//!
//! This is the whole of "Check for updates", and what it deliberately is not is
//! an updater. It does not download, verify a signature, or replace anything on
//! disk — a self-updater that can write to its own bundle is a large security
//! surface, and one that is not signed is a worse one. This answers a question
//! and hands the user a link.
//!
//! ## Three answers, not two
//!
//! The interesting states are not "current" and "outdated". They are:
//!
//! - **a newer release exists**, which is the only one that asks anything of
//!   the user;
//! - **this is the newest**, which is a real answer;
//! - **the project has published no releases at all**, which is where this
//!   repository actually is today and which the GitHub API reports as a plain
//!   404. Rendering that as an error would tell the user something is broken
//!   when nothing is, and rendering it as "up to date" would be a claim the
//!   API never made.
//!
//! A network failure is a fourth state and stays distinct from all of them: not
//! knowing is not the same as knowing there is nothing.

use serde::{Deserialize, Serialize};

/// Where releases are published, and where the user is sent to read them.
const REPOSITORY: &str = "jbelke/RDirStat";

/// GitHub requires a User-Agent on every API request and 403s without one.
const USER_AGENT: &str = concat!("rdirstat/", env!("CARGO_PKG_VERSION"));

/// How long to wait before giving up. Short: this runs behind a button the user
/// pressed and is watching, and a check that hangs for a minute reads as broken.
const TIMEOUT_SECONDS: u64 = 10;

/// What a release check found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct ReleaseCheck {
    /// The running version, from the crate manifest.
    pub current: String,
    /// The newest published tag, or `None` when nothing is published yet.
    pub latest: Option<String>,
    /// True only when `latest` is genuinely ahead of `current`.
    pub newer_available: bool,
    /// Where a human should go to read about it.
    pub releases_url: String,
}

/// The one field this needs out of GitHub's release JSON.
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

/// The running version.
pub(crate) fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Where a human reads the release notes.
pub(crate) fn releases_url() -> String {
    format!("https://github.com/{REPOSITORY}/releases")
}

/// Splits a version into its numeric parts and whether it is a pre-release.
///
/// Tolerant on purpose: this parses a string a stranger typed into a git tag,
/// not a value this program produced. A leading `v`, extra components, and
/// trailing junk on a component all have obvious readings, and the alternative
/// to taking them is refusing to answer at all.
fn parts(version: &str) -> (Vec<u64>, bool) {
    let trimmed = version.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match trimmed.split_once(['-', '+']) {
        Some((core, _)) => (core, true),
        None => (trimmed, false),
    };
    let numbers = core
        .split('.')
        .map(|component| {
            let digits: String = component.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect();
    (numbers, pre)
}

/// True when `latest` is a strictly newer version than `current`.
///
/// Missing components read as zero, so `1.2` and `1.2.0` are the same version
/// rather than one being mysteriously ahead. A pre-release loses to the release
/// it shares numbers with — `1.2.0-rc1` is not an upgrade from `1.2.0` — which
/// is the one rule that stops a release-candidate tag from nagging everybody
/// who is already running the final build.
pub(crate) fn is_newer(latest: &str, current: &str) -> bool {
    let (left, left_pre) = parts(latest);
    let (right, right_pre) = parts(current);
    let width = left.len().max(right.len());
    for index in 0..width {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    // Same numbers: only a release beats a pre-release.
    right_pre && !left_pre
}

/// Asks GitHub for the newest release.
///
/// # Errors
///
/// The transport failed, timed out, or answered with something that is neither
/// a release nor a 404. A 404 is not an error — it is "nothing published yet"
/// and comes back as `Ok(None)`.
pub(crate) async fn check() -> Result<ReleaseCheck, String> {
    let current = current_version().to_owned();
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECONDS))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|error| format!("could not start the check: {error}"))?;

    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| format!("could not reach GitHub: {error}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(ReleaseCheck {
            current,
            latest: None,
            newer_available: false,
            releases_url: releases_url(),
        });
    }
    if !response.status().is_success() {
        return Err(format!("GitHub answered {}", response.status()));
    }

    let release: GithubRelease = response
        .json()
        .await
        .map_err(|error| format!("could not read GitHub's answer: {error}"))?;

    let newer_available = is_newer(&release.tag_name, &current);
    Ok(ReleaseCheck {
        current,
        latest: Some(release.tag_name),
        newer_available,
        releases_url: releases_url(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_number_anywhere_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn a_leading_v_is_not_part_of_the_version() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("V0.1.0", "v0.1.0"));
    }

    /// Otherwise `1.2` looks like an upgrade from `1.2.0`, or the reverse,
    /// depending on which side happened to be written out in full.
    #[test]
    fn a_missing_component_is_zero_not_a_difference() {
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("1.2.0", "1.2"));
        assert!(is_newer("1.2.1", "1.2"));
    }

    /// A release candidate must not nag someone already on the final build.
    #[test]
    fn a_pre_release_does_not_beat_the_release_it_shares_numbers_with() {
        assert!(!is_newer("1.2.0-rc1", "1.2.0"));
        assert!(is_newer("1.2.0", "1.2.0-rc1"));
        assert!(is_newer("1.3.0-rc1", "1.2.0"));
    }

    /// Tags are written by people. Refusing to answer is worse than reading
    /// the obvious intent.
    #[test]
    fn junk_on_a_component_does_not_break_the_comparison() {
        assert!(is_newer("0.2.0-beta", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
        assert!(is_newer("0.1.0", ""));
    }

    #[test]
    fn the_running_version_is_the_manifest_version() {
        assert_eq!(current_version(), env!("CARGO_PKG_VERSION"));
        assert!(releases_url().starts_with("https://github.com/"));
    }
}
