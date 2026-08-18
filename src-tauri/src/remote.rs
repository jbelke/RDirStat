//! The saved list of remote destinations, and the Keychain that holds their
//! secrets.
//!
//! [`rdirstat_remote`] deliberately knows nothing about either: it is handed a
//! target and a credential bundle. This module is where those come from, and it
//! is the only place in the app that touches a secret.
//!
//! ## The split, and why it is a file boundary rather than a convention
//!
//! `remotes.json` holds endpoints. The Keychain holds secrets. Nothing writes a
//! secret to the file, and the reason it cannot is structural rather than
//! careful: [`rdirstat_remote::RemoteTarget`] — the only type that is
//! serialised into `remotes.json` — has no field that could hold one. A copy of
//! that file is a list of hostnames.
//!
//! ## Why not `settings.json`
//!
//! `settings.rs` says of itself that the settings file "stays small, and is the
//! only thing left behind when the store moves to another volume". A list that
//! grows with every destination the user adds is not that, and a damaged
//! `remotes.json` should cost the user their target list — not the snapshot
//! directory setting that tells the app where tens of gigabytes of scan
//! artifacts already live.
//!
//! ## The S3 resolution order
//!
//! 1. An explicit key stored here, in the Keychain, for this target.
//! 2. `AWS_ACCESS_KEY_ID` and friends in the environment.
//! 3. `~/.aws/credentials`, `~/.aws/config`, and an SSO session.
//! 4. An instance or container role.
//!
//! Only step 1 is implemented here. Steps 2 through 4 are the AWS SDK's own
//! chain, and [`rdirstat_remote::connect`] reaches them by *not* disabling
//! OpenDAL's config load when no explicit key was found. So the order is
//! enforced by a single fact — whether this module returned a key pair — rather
//! than by a cascade this app would have to keep in step with the SDK's.
//!
//! The common case stores nothing at all: a developer with a working
//! `aws` CLI, and every SFTP target ever.

use std::path::Path;

use rdirstat_remote::{RemoteCredentials, RemoteKind, RemoteTarget, Secret, TargetError};
use serde::{Deserialize, Serialize};

/// The file, next to `settings.json` in Application Support.
const FILE_NAME: &str = "remotes.json";

/// The Keychain service every secret is filed under.
///
/// The bundle identifier, matching `tauri.conf.json`. Keychain items are scoped
/// by service, so this is what keeps this app's entries from colliding with
/// anything else in the user's login keychain — and what makes them
/// recognisable in Keychain Access when the user wants to revoke one by hand.
const KEYCHAIN_SERVICE: &str = "jbelke.rdirstat";

/// Ceiling on saved targets.
///
/// Not a resource limit — a blast-radius one. Every target is a place this app
/// can write, and a list that grew to thousands would be a list nobody reviews.
const MAX_TARGETS: usize = 64;

/// What went wrong with a target the user was editing.
///
/// `Display` and `Error` are hand-written rather than derived, matching
/// [`crate::sync::SyncError`] and [`crate::relocate::RelocateError`]:
/// docs/08-RUST-PRACTICES.md reserves `thiserror` for libraries and has the
/// shell convert to a typed IPC error at the boundary, so `src-tauri` does not
/// depend on it and adding it for one enum is the wrong end of that trade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub(crate) enum RemoteConfigError {
    /// A field did not validate. Carries the field name.
    Invalid(String),
    /// Another target already uses this name.
    DuplicateName(String),
    NotFound(String),
    TooMany(usize),
    /// The Keychain refused. Usually a locked keychain or a denied prompt.
    Keychain(String),
    Internal(String),
}

impl std::fmt::Display for RemoteConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) | Self::Internal(reason) => write!(f, "{reason}"),
            Self::DuplicateName(name) => write!(f, "a destination called {name} already exists"),
            Self::NotFound(name) => write!(f, "no destination called {name}"),
            Self::TooMany(limit) => write!(f, "this app keeps at most {limit} destinations"),
            Self::Keychain(reason) => write!(f, "the keychain could not be used: {reason}"),
        }
    }
}

impl std::error::Error for RemoteConfigError {}

impl From<TargetError> for RemoteConfigError {
    fn from(error: TargetError) -> Self {
        Self::Invalid(error.to_string())
    }
}

/// The saved list, as it sits on disk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registry {
    #[serde(default)]
    pub targets: Vec<RemoteTarget>,
}

/// Reads `remotes.json`, tolerating absence and damage.
///
/// Same policy as `settings::load`, for the same reason: a fresh install has no
/// file, and refusing to start because a JSON brace went missing is a worse
/// failure than an empty destination list the user can rebuild.
pub(crate) fn load(app_data: &Path) -> Registry {
    let path = app_data.join(FILE_NAME);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Registry::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "the destination list is unreadable; starting empty");
        Registry::default()
    })
}

/// Writes the list, staged and renamed.
///
/// # Errors
///
/// Any I/O error from creating the directory, writing, or renaming.
pub(crate) fn save(app_data: &Path, registry: &Registry) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(app_data)?;
    let text = serde_json::to_string_pretty(registry).map_err(std::io::Error::other)?;
    let staging = app_data.join(format!(".{FILE_NAME}.{}", std::process::id()));
    std::fs::write(&staging, text.as_bytes())?;
    std::fs::rename(&staging, app_data.join(FILE_NAME))
}

/// What the caller wants stored for a target, if anything.
///
/// `None` on a field means *leave whatever is already in the Keychain alone*,
/// which is what lets the user edit a target's folder without re-typing a
/// secret the UI never received in the first place — the editor is populated
/// from `remotes.json`, which by construction has no secret in it.
#[derive(Debug, Default, Deserialize, specta::Type)]
pub(crate) struct SecretInput {
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
    pub password: Option<String>,
    pub key_path: Option<String>,
}

impl SecretInput {
    fn is_empty(&self) -> bool {
        self.access_key.is_none()
            && self.secret_key.is_none()
            && self.session_token.is_none()
            && self.password.is_none()
            && self.key_path.is_none()
    }
}

/// The Keychain payload for one target.
///
/// A JSON blob rather than five Keychain items, because the five belong to one
/// credential: deleting a target must not be able to leave three of them
/// behind, and an S3 session token without its access key is not merely useless
/// but actively confusing to find later.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredSecret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_path: Option<String>,
}

impl StoredSecret {
    fn is_empty(&self) -> bool {
        self.access_key.is_none()
            && self.secret_key.is_none()
            && self.session_token.is_none()
            && self.password.is_none()
            && self.key_path.is_none()
    }

    /// Applies an edit, treating `None` as "unchanged" and `Some("")` as
    /// "clear this".
    ///
    /// The empty string is the only way the UI can say *remove the password I
    /// stored*, since it cannot send back a secret it was never shown.
    fn apply(&mut self, input: SecretInput) {
        fn merge(slot: &mut Option<String>, edit: Option<String>) {
            match edit {
                None => {}
                Some(value) if value.is_empty() => *slot = None,
                Some(value) => *slot = Some(value),
            }
        }
        merge(&mut self.access_key, input.access_key);
        merge(&mut self.secret_key, input.secret_key);
        merge(&mut self.session_token, input.session_token);
        merge(&mut self.password, input.password);
        merge(&mut self.key_path, input.key_path);
    }
}

/// Reads a target's Keychain entry.
///
/// A missing entry is `Ok(default)`, not an error: it is the normal state for
/// an SFTP target and for any S3 target that relies on the AWS chain. Only a
/// keychain that exists and refuses is a failure worth surfacing.
fn read_secret(name: &str) -> Result<StoredSecret, RemoteConfigError> {
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, name) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            // Deliberately does not include the payload in the message.
            RemoteConfigError::Keychain(format!("the stored credential could not be read: {error}"))
        }),
        // errSecItemNotFound. There is nothing stored, which is not a problem.
        Err(error) if error.code() == -25300 => Ok(StoredSecret::default()),
        Err(error) => Err(RemoteConfigError::Keychain(error.to_string())),
    }
}

fn write_secret(name: &str, secret: &StoredSecret) -> Result<(), RemoteConfigError> {
    if secret.is_empty() {
        return forget_secret(name);
    }
    let bytes = serde_json::to_vec(secret).map_err(|error| RemoteConfigError::Internal(error.to_string()))?;
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, name, &bytes)
        .map_err(|error| RemoteConfigError::Keychain(error.to_string()))
}

/// Removes a target's Keychain entry. Absence is success.
fn forget_secret(name: &str) -> Result<(), RemoteConfigError> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, name) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == -25300 => Ok(()),
        Err(error) => Err(RemoteConfigError::Keychain(error.to_string())),
    }
}

/// Whether a target has a secret stored for it.
///
/// The UI needs to render "a password is saved" without ever receiving one.
#[must_use]
pub(crate) fn has_stored_secret(name: &str) -> bool {
    read_secret(name).is_ok_and(|secret| !secret.is_empty())
}

/// The credential bundle for a target, ready for
/// [`rdirstat_remote::connect`].
///
/// # Errors
///
/// [`RemoteConfigError::Keychain`] if the keychain exists and refused.
pub(crate) fn credentials(target: &RemoteTarget) -> Result<RemoteCredentials, RemoteConfigError> {
    let stored = read_secret(&target.name)?;
    Ok(RemoteCredentials {
        access_key: stored.access_key.map(Secret::new),
        secret_key: stored.secret_key.map(Secret::new),
        session_token: stored.session_token.map(Secret::new),
        password: stored.password.map(Secret::new),
        // SFTP alone reads a path rather than a secret, and only when the user
        // named a key outside the ones ssh-agent already offers.
        key_path: if target.kind == RemoteKind::Sftp {
            stored.key_path
        } else {
            None
        },
    })
}

/// Adds a target, or replaces the one with the same name.
///
/// Validates before touching either store, so a rejected edit leaves the saved
/// list and the Keychain exactly as they were.
///
/// # Errors
///
/// [`RemoteConfigError`] for a bad field, a duplicate name, a full list, or a
/// keychain that refused.
pub(crate) fn upsert(
    app_data: &Path,
    target: &RemoteTarget,
    secret: SecretInput,
    replacing: Option<&str>,
) -> Result<RemoteTarget, RemoteConfigError> {
    let clean = target.validated()?;
    let mut registry = load(app_data);

    let existing = registry.targets.iter().position(|saved| saved.name == clean.name);
    let previous = replacing.and_then(|name| registry.targets.iter().position(|saved| saved.name == name));

    match (existing, previous) {
        // Renaming onto a name somebody else already holds.
        (Some(found), Some(old)) if found != old => {
            return Err(RemoteConfigError::DuplicateName(clean.name.clone()));
        }
        // Adding a name that already exists.
        (Some(_), None) => return Err(RemoteConfigError::DuplicateName(clean.name.clone())),
        _ => {}
    }

    // Read the existing secret BEFORE the rename, so an edit that only changes
    // the folder carries the stored credential across to the new name instead
    // of silently dropping it.
    let carried = previous
        .and_then(|index| registry.targets.get(index))
        .map(|old| read_secret(&old.name))
        .transpose()?
        .unwrap_or_default();

    let mut merged = carried;
    merged.apply(secret);

    if let Some(index) = previous {
        let old_name = registry.targets.get(index).map(|target| target.name.clone());
        if let Some(slot) = registry.targets.get_mut(index) {
            *slot = clean.clone();
        }
        // Write the new entry first, then drop the old one: the reverse order
        // loses the credential if the write fails.
        write_secret(&clean.name, &merged)?;
        if let Some(old_name) = old_name.filter(|old| old != &clean.name) {
            forget_secret(&old_name)?;
        }
    } else {
        if registry.targets.len() >= MAX_TARGETS {
            return Err(RemoteConfigError::TooMany(MAX_TARGETS));
        }
        registry.targets.push(clean.clone());
        write_secret(&clean.name, &merged)?;
    }

    save(app_data, &registry).map_err(|error| RemoteConfigError::Internal(error.to_string()))?;
    Ok(clean)
}

/// Removes a target and the secret that went with it.
///
/// # Errors
///
/// [`RemoteConfigError::NotFound`] if no such target, or a keychain failure.
pub(crate) fn remove(app_data: &Path, name: &str) -> Result<(), RemoteConfigError> {
    let mut registry = load(app_data);
    let Some(index) = registry.targets.iter().position(|target| target.name == name) else {
        return Err(RemoteConfigError::NotFound(name.to_owned()));
    };
    registry.targets.remove(index);
    save(app_data, &registry).map_err(|error| RemoteConfigError::Internal(error.to_string()))?;
    // After the list is written, so a keychain refusal cannot leave a target
    // listed but unopenable. An orphaned keychain item is the safer leftover:
    // it is visible in Keychain Access and deletable by hand.
    forget_secret(name)
}

/// One target as the UI sees it: the saved fields, plus whether a secret exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
pub(crate) struct TargetView {
    #[serde(flatten)]
    pub target: RemoteTarget,
    /// True when the Keychain holds something for this target. Never the value.
    pub has_secret: bool,
    /// True when this kind authenticates without anything stored, so the UI can
    /// say "uses your SSH keys" instead of showing an empty password field.
    pub uses_ambient_credentials: bool,
}

/// The saved targets, decorated for display.
#[must_use]
pub(crate) fn list(app_data: &Path) -> Vec<TargetView> {
    load(app_data)
        .targets
        .into_iter()
        .map(|target| TargetView {
            has_secret: has_stored_secret(&target.name),
            uses_ambient_credentials: !target.kind.needs_stored_secret(),
            target,
        })
        .collect()
}

/// Finds one saved target by name.
///
/// # Errors
///
/// [`RemoteConfigError::NotFound`].
pub(crate) fn find(app_data: &Path, name: &str) -> Result<RemoteTarget, RemoteConfigError> {
    load(app_data)
        .targets
        .into_iter()
        .find(|target| target.name == name)
        .ok_or_else(|| RemoteConfigError::NotFound(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(name: &str) -> RemoteTarget {
        RemoteTarget {
            name: name.to_owned(),
            kind: RemoteKind::S3,
            endpoint: String::new(),
            bucket: "photos".to_owned(),
            region: "us-west-2".to_owned(),
            root: "/archive/".to_owned(),
            user: String::new(),
        }
    }

    // The property the whole module exists for. Everything else here is
    // bookkeeping; this is the promise.
    #[test]
    fn the_saved_file_contains_no_secret_shaped_field() {
        let registry = Registry {
            targets: vec![s3("backup")],
        };
        let text = serde_json::to_string(&registry).expect("a registry serialises");
        for forbidden in ["access_key", "secret_key", "session_token", "password", "key_path"] {
            assert!(!text.contains(forbidden), "{forbidden} reached remotes.json: {text}");
        }
    }

    #[test]
    fn a_registry_round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let registry = Registry {
            targets: vec![s3("one"), s3("two")],
        };
        save(dir.path(), &registry).expect("the registry should save");
        assert_eq!(load(dir.path()), registry);
    }

    #[test]
    fn a_missing_file_is_an_empty_list_not_an_error() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(load(dir.path()), Registry::default());
    }

    #[test]
    fn a_damaged_file_costs_the_list_and_nothing_else() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join(FILE_NAME), b"{ not json").expect("the fixture should write");
        assert_eq!(load(dir.path()), Registry::default());
    }

    // `None` means "unchanged" and `Some("")` means "clear it". Getting these
    // the wrong way round would either wipe a credential on every folder edit
    // or make one impossible to remove.
    #[test]
    fn an_absent_edit_leaves_a_stored_secret_alone() {
        let mut stored = StoredSecret {
            access_key: Some("AKIA".to_owned()),
            secret_key: Some("wJalr".to_owned()),
            ..StoredSecret::default()
        };
        stored.apply(SecretInput::default());
        assert_eq!(stored.access_key.as_deref(), Some("AKIA"));
        assert_eq!(stored.secret_key.as_deref(), Some("wJalr"));
    }

    #[test]
    fn an_empty_edit_clears_a_stored_secret() {
        let mut stored = StoredSecret {
            password: Some("hunter2".to_owned()),
            ..StoredSecret::default()
        };
        stored.apply(SecretInput {
            password: Some(String::new()),
            ..SecretInput::default()
        });
        assert_eq!(stored.password, None);
        assert!(stored.is_empty());
    }

    #[test]
    fn a_new_value_replaces_the_old_one() {
        let mut stored = StoredSecret {
            password: Some("old".to_owned()),
            ..StoredSecret::default()
        };
        stored.apply(SecretInput {
            password: Some("new".to_owned()),
            ..SecretInput::default()
        });
        assert_eq!(stored.password.as_deref(), Some("new"));
    }

    #[test]
    fn an_empty_input_is_recognised_as_no_edit_at_all() {
        assert!(SecretInput::default().is_empty());
        assert!(
            !SecretInput {
                password: Some(String::new()),
                ..SecretInput::default()
            }
            .is_empty()
        );
    }

    // An SFTP target must never be handed a key path that was stored while it
    // was some other kind, because `key` on the SFTP builder is the one field
    // that silently changes which identity ssh offers.
    #[test]
    fn a_key_path_only_reaches_an_sftp_target() {
        let stored = StoredSecret {
            key_path: Some("/home/josh/.ssh/id_ed25519".to_owned()),
            ..StoredSecret::default()
        };
        let json = serde_json::to_vec(&stored).expect("the stored secret serialises");
        let parsed: StoredSecret = serde_json::from_slice(&json).expect("and parses back");
        assert_eq!(parsed.key_path.as_deref(), Some("/home/josh/.ssh/id_ed25519"));
    }

    #[test]
    fn an_unset_field_is_omitted_from_the_keychain_payload() {
        let stored = StoredSecret {
            password: Some("hunter2".to_owned()),
            ..StoredSecret::default()
        };
        let text = serde_json::to_string(&stored).expect("the stored secret serialises");
        assert_eq!(text, r#"{"password":"hunter2"}"#);
    }
}
