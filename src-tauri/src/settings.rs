//! Where the app's own knobs live, and the one knob that exists so far: which
//! directory the snapshot store writes into.
//!
//! ## Why the store is configurable at all
//!
//! The default — `<app data>/snapshots` under Application Support — puts tens
//! of gigabytes of scan artifacts on the boot volume, which is the volume most
//! likely to be small and the one the user is usually trying to free up. A
//! machine with a large external disk should be able to keep its scan cache
//! there. That is a location preference, not a format change: the `*.rdstat`
//! files, the pruning rule and the atomic-rename protocol are identical
//! wherever the root is.
//!
//! ## The resolution order, and why it is this way round
//!
//! 1. `RDIRSTAT_DATA_DIR` in the process environment.
//! 2. `snapshot_dir` in the settings file.
//! 3. `<app data>/snapshots`.
//!
//! The environment wins because that is what an environment variable is *for*:
//! a per-run override that a script, a test, or a developer can set without
//! mutating the user's saved state. The important consequence is that when the
//! variable is set the settings file is **not silently ignored** — it is still
//! read, and [`SnapshotRoot::source`] reports which layer actually won, so the
//! UI can grey the control out and say why rather than appearing to accept an
//! edit that does nothing.
//!
//! ## Why the settings file cannot live in the store
//!
//! It records *where the store is*. Putting it inside the store would mean
//! having to know the answer before you can read it. So `settings.json` stays
//! in Application Support next to nothing else, stays small, and is the only
//! thing left behind when the store moves to another volume.
//!
//! ## `.env`
//!
//! [`load_dotenv`] is a development convenience and is documented as one. A
//! bundled `.app` is launched by `launchd` with a working directory of `/`, so
//! there is no project `.env` to find and the settings file is the only
//! mechanism that applies to a real install. In a `cargo tauri dev` run the
//! working directory is `src-tauri/`, so both it and the repository root are
//! checked. Real environment variables always win over the file, which is the
//! usual dotenv rule and the one that keeps `RDIRSTAT_DATA_DIR=… cargo run`
//! working.
//!
//! We parse the handful of lines ourselves rather than take a dependency for
//! it. docs/08-RUST-PRACTICES.md#dependency-discipline is explicit that this
//! workspace does not add crates casually, and the subset below — `KEY=VALUE`,
//! comments, blank lines, optional surrounding quotes — is the whole of what a
//! path assignment needs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The environment variable that overrides the store location.
pub(crate) const DATA_DIR_ENV: &str = "RDIRSTAT_DATA_DIR";

/// The settings file, under the app data directory.
const FILE_NAME: &str = "settings.json";

/// Everything the app persists that is not a scan.
///
/// `#[serde(default)]` on the struct and `skip_serializing_if` on the field
/// mean an older file missing this key still loads, and a settings file with
/// nothing set is written as `{}` rather than as a null that a future reader
/// would have to special-case.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    /// An absolute directory the user chose for the snapshot store, or `None`
    /// to use the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_dir: Option<PathBuf>,
}

/// Which layer of the resolution order supplied the store root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RootSource {
    /// `RDIRSTAT_DATA_DIR` was set, so the saved setting is inert this run.
    Environment,
    /// The user chose a directory and it is in effect.
    Setting,
    /// Nothing was configured; this is `<app data>/snapshots`.
    Default,
}

/// The resolved store root and the reason it is that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotRoot {
    pub(crate) path: PathBuf,
    pub(crate) source: RootSource,
    /// What the store root would be with nothing configured. Carried so the
    /// UI can offer "reset to default" without recomputing it, and so an error
    /// message can name the fallback.
    pub(crate) default_path: PathBuf,
    /// The saved setting, whether or not it won. A UI that hid this while the
    /// environment overrode it would be lying about what it will do tomorrow.
    pub(crate) configured: Option<PathBuf>,
}

impl SnapshotRoot {
    /// A default-sourced root at an explicit path. For tests, which are about
    /// what [`crate::storage::describe`] reports rather than about which layer
    /// chose the directory.
    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            default_path: path.clone(),
            path,
            source: RootSource::Default,
            configured: None,
        }
    }
}

/// Reads `settings.json` from `app_data`, tolerating absence and damage.
///
/// A missing file is the ordinary state of a fresh install. A corrupt one is
/// logged and treated as empty rather than propagated: refusing to launch
/// because a preferences file lost a brace would be a worse failure than
/// forgetting a preference.
pub(crate) fn load(app_data: &Path) -> Settings {
    let path = app_data.join(FILE_NAME);
    let Ok(text) = fs::read_to_string(&path) else {
        return Settings::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "settings file is unreadable; using defaults");
        Settings::default()
    })
}

/// Writes `settings` to `app_data/settings.json`.
///
/// Staged and renamed, for the same reason snapshots are: a half-written
/// preferences file that survives a crash is one that has to be hand-repaired.
///
/// # Errors
///
/// Any I/O error from creating the directory, writing, or renaming.
pub(crate) fn save(app_data: &Path, settings: &Settings) -> Result<(), std::io::Error> {
    fs::create_dir_all(app_data)?;
    let text = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    let staging = app_data.join(format!(".{FILE_NAME}.{}", std::process::id()));
    fs::write(&staging, text.as_bytes())?;
    fs::rename(&staging, app_data.join(FILE_NAME))
}

/// Applies the resolution order documented above.
///
/// `app_data` is the platform data directory; the default root is `snapshots`
/// beneath it. Neither this function nor its callers create anything — that is
/// [`crate::snapshot_store::SnapshotStore::new`]'s job, so a caller that only
/// wants to *describe* the configuration cannot accidentally materialise a
/// directory on a volume the user has not agreed to write to.
pub(crate) fn resolve_root(app_data: &Path) -> SnapshotRoot {
    choose(app_data.join("snapshots"), load(app_data).snapshot_dir, env_override())
}

/// The precedence rule itself, with the two lookups already done.
///
/// Split out because it is the part with the behaviour worth testing, and
/// testing it through [`resolve_root`] would mean mutating the process
/// environment — which is `unsafe` in edition 2024 and unsound against any
/// other test thread that reads it.
fn choose(default_path: PathBuf, configured: Option<PathBuf>, from_env: Option<PathBuf>) -> SnapshotRoot {
    let (path, source) = match (from_env, configured.clone()) {
        (Some(path), _) => (path, RootSource::Environment),
        (None, Some(path)) => (path, RootSource::Setting),
        (None, None) => (default_path.clone(), RootSource::Default),
    };
    SnapshotRoot {
        path,
        source,
        default_path,
        configured,
    }
}

/// Values parsed from `.env`, consulted only where the real environment is
/// silent. Empty until [`load_dotenv`] runs, which is once, from `run`.
static DOTENV: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// `RDIRSTAT_DATA_DIR`, if it is set to something usable.
///
/// The real environment is checked first and `.env` second, which is the usual
/// dotenv precedence and what keeps `RDIRSTAT_DATA_DIR=… cargo run` working in
/// a checkout that also has a `.env`.
///
/// An empty or whitespace-only value is treated as unset rather than as a
/// request to use the current directory, because `RDIRSTAT_DATA_DIR=` in a
/// `.env` file means "I commented this out", not "put the store in `.`".
fn env_override() -> Option<PathBuf> {
    let from_process = std::env::var(DATA_DIR_ENV).ok();
    let raw = match from_process {
        Some(value) if !value.trim().is_empty() => Some(value),
        _ => DOTENV.get().and_then(|map| map.get(DATA_DIR_ENV).cloned()),
    };
    usable(raw.as_deref())
}

/// A configured value, or `None` if it is blank.
///
/// An empty or whitespace-only value is treated as unset rather than as a
/// request to use the current directory, because `RDIRSTAT_DATA_DIR=` in a
/// `.env` file means "I commented this out", not "put the store in `.`".
fn usable(raw: Option<&str>) -> Option<PathBuf> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Parses `.env` into [`DOTENV`]. Development convenience; see the module docs.
///
/// Deliberately does **not** call `std::env::set_var`. That function is
/// `unsafe` in edition 2024 because it is unsound against any concurrently
/// running thread that reads the environment, and importing the workspace's
/// first `unsafe` block for a developer convenience would be a poor trade when
/// a lookup table does the same job. [`env_override`] consults this map only
/// where the real environment is silent, which is the semantics `set_var`
/// would have given us anyway.
///
/// Checks the working directory and then its parent, which covers both
/// `cargo tauri dev` (running in `src-tauri/`) and a plain `cargo run` from the
/// repository root. Returns the file it used, for the startup log line. Calling
/// it more than once is harmless and has no effect after the first.
pub(crate) fn load_dotenv() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let candidates = [cwd.join(".env"), cwd.parent()?.join(".env")];
    let path = candidates.into_iter().find(|path| path.is_file())?;
    let text = fs::read_to_string(&path).ok()?;
    let parsed: BTreeMap<String, String> = parse_dotenv(&text).into_iter().collect();
    DOTENV.set(parsed).ok()?;
    Some(path)
}

/// `KEY=VALUE` pairs from a `.env` body.
///
/// Skips blank lines, `#` comments, and anything without an `=`. Strips one
/// layer of matching single or double quotes from the value, which is what a
/// path containing a space needs and the only escaping this subset supports.
/// An `export ` prefix is tolerated so a file that is also `source`-able works.
fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_owned(), unquote(value.trim()).to_owned()))
        })
        .collect()
}

/// Removes one layer of matching quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)) {
            return inner;
        }
    }
    value
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_settings_file_is_defaults_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(load(dir.path()), Settings::default());
    }

    #[test]
    fn a_corrupt_settings_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join(FILE_NAME), b"{ not json").expect("write");
        assert_eq!(load(dir.path()), Settings::default());
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let settings = Settings {
            snapshot_dir: Some(PathBuf::from("/Volumes/big/snapshots")),
        };
        save(dir.path(), &settings).expect("save");
        assert_eq!(load(dir.path()), settings);
    }

    #[test]
    fn an_unset_snapshot_dir_is_not_written_as_null() {
        let dir = tempfile::tempdir().expect("temp dir");
        save(dir.path(), &Settings::default()).expect("save");
        let text = fs::read_to_string(dir.path().join(FILE_NAME)).expect("read");
        assert!(!text.contains("null"), "{text}");
    }

    #[test]
    fn the_default_root_is_snapshots_under_app_data() {
        let dir = tempfile::tempdir().expect("temp dir");
        let resolved = resolve_root(dir.path());
        assert_eq!(resolved.source, RootSource::Default);
        assert_eq!(resolved.path, dir.path().join("snapshots"));
        assert_eq!(resolved.configured, None);
    }

    #[test]
    fn a_saved_setting_beats_the_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let chosen = PathBuf::from("/Volumes/big/snapshots");
        save(
            dir.path(),
            &Settings {
                snapshot_dir: Some(chosen.clone()),
            },
        )
        .expect("save");

        let resolved = resolve_root(dir.path());
        assert_eq!(resolved.source, RootSource::Setting);
        assert_eq!(resolved.path, chosen);
        assert_eq!(resolved.default_path, dir.path().join("snapshots"));
    }

    #[test]
    fn resolving_creates_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let resolved = resolve_root(dir.path());
        assert!(!resolved.path.exists(), "resolve_root must not materialise a directory");
    }

    #[test]
    fn the_environment_beats_a_saved_setting_but_does_not_erase_it() {
        // The UI has to be able to say "your saved folder is X, but
        // RDIRSTAT_DATA_DIR is pointing us at Y this run". Dropping the saved
        // value here would make that impossible to render.
        let resolved = choose(
            PathBuf::from("/app-data/snapshots"),
            Some(PathBuf::from("/Volumes/big/snapshots")),
            Some(PathBuf::from("/tmp/override")),
        );
        assert_eq!(resolved.source, RootSource::Environment);
        assert_eq!(resolved.path, PathBuf::from("/tmp/override"));
        assert_eq!(resolved.configured, Some(PathBuf::from("/Volumes/big/snapshots")));
    }

    #[test]
    fn a_blank_environment_value_is_unset_not_the_current_directory() {
        assert_eq!(usable(Some("")), None);
        assert_eq!(usable(Some("   ")), None);
        assert_eq!(usable(None), None);
        assert_eq!(
            usable(Some("  /Volumes/big/snapshots  ")),
            Some(PathBuf::from("/Volumes/big/snapshots"))
        );
    }

    #[test]
    fn the_default_is_carried_even_when_something_else_wins() {
        // "Reset to default" needs the default path to still be in the report.
        let resolved = choose(
            PathBuf::from("/app-data/snapshots"),
            Some(PathBuf::from("/Volumes/big/snapshots")),
            None,
        );
        assert_eq!(resolved.source, RootSource::Setting);
        assert_eq!(resolved.default_path, PathBuf::from("/app-data/snapshots"));
    }
    #[test]
    fn dotenv_parsing_covers_comments_quotes_and_export() {
        let parsed = parse_dotenv(
            "# a comment\n\nexport RDIRSTAT_DATA_DIR=\"/Volumes/my disk/snapshots\"\nOTHER='x'\nnot a pair\n=novalue\n",
        );
        assert_eq!(
            parsed,
            vec![
                ("RDIRSTAT_DATA_DIR".to_owned(), "/Volumes/my disk/snapshots".to_owned()),
                ("OTHER".to_owned(), "x".to_owned()),
            ]
        );
    }
}
