//! The secret half of a connection, kept apart from the saved half.
//!
//! [`RemoteCredentials`] is constructed at connect time, lives as long as the
//! call, and is never serialised. It deliberately does **not** derive
//! `Serialize`, `Deserialize`, `Debug` or `Clone`-into-a-log: the compiler is a
//! better guarantee that a secret stays out of `settings.json` and out of a
//! `tracing::warn!` than a comment asking future callers not to put it there.
//!
//! Where the values come from is `src-tauri`'s business — the macOS Keychain
//! for an explicitly entered key, the environment and `~/.aws/credentials` for
//! the AWS chain, nothing at all for SFTP. This crate only says what shape it
//! needs and what it does when a field is absent.

/// A secret string that will not print itself.
///
/// `Debug` renders the length, never the value. This is the difference between
/// an error path that logs "connect failed for target backup" and one that
/// logs a live access key into `~/Library/Logs`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The only way to read it. Named so that a call site reads as a
    /// deliberate act at review time.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} chars)", self.0.len())
    }
}

/// Everything secret a connection might need, all of it optional.
///
/// Optional because the common case supplies none of it. An S3 target on a
/// machine with `~/.aws/credentials`, an SSO session or an IAM role needs no
/// stored key, and an SFTP target needs nothing ever. `Default` is therefore
/// the *normal* value, not a placeholder.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct RemoteCredentials {
    /// S3 access key id. Paired with `secret_key`; one without the other is
    /// treated as neither.
    pub access_key: Option<Secret>,
    pub secret_key: Option<Secret>,
    /// S3 session token, for temporary credentials from SSO or `AssumeRole`.
    pub session_token: Option<Secret>,
    /// WebDAV password, for the target's `user`.
    pub password: Option<Secret>,
    /// SFTP private key path.
    ///
    /// A **path**, not a key: handing OpenDAL a filename lets `ssh` read it
    /// with the permissions the user already granted, and keeps this process
    /// from ever holding private key material. `None` — the normal case —
    /// means ssh-agent and `~/.ssh/config` decide, exactly as they do for
    /// `ssh` on the command line.
    pub key_path: Option<String>,
}

impl RemoteCredentials {
    /// True when an explicit S3 key pair is present and usable.
    ///
    /// Both halves or neither. A target holding an access key with no secret
    /// would otherwise be signed with an empty secret and rejected by the
    /// server as a *signature* failure, which sends the user looking at their
    /// clock and their region instead of at the missing field.
    #[must_use]
    pub fn has_s3_key_pair(&self) -> bool {
        matches!(
            (&self.access_key, &self.secret_key),
            (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_does_not_print_itself() {
        let rendered = format!("{:?}", Secret::new("AKIAIOSFODNN7EXAMPLE"));
        assert!(!rendered.contains("AKIA"), "{rendered}");
        assert_eq!(rendered, "Secret(20 chars)");
    }

    #[test]
    fn a_credential_bundle_does_not_print_its_contents() {
        let rendered = format!(
            "{:?}",
            RemoteCredentials {
                secret_key: Some(Secret::new("wJalrXUtnFEMI")),
                ..RemoteCredentials::default()
            }
        );
        assert!(!rendered.contains("wJalr"), "{rendered}");
    }

    #[test]
    fn half_a_key_pair_is_no_key_pair() {
        let only_access = RemoteCredentials {
            access_key: Some(Secret::new("AKIA")),
            ..RemoteCredentials::default()
        };
        assert!(!only_access.has_s3_key_pair());

        let empty_secret = RemoteCredentials {
            access_key: Some(Secret::new("AKIA")),
            secret_key: Some(Secret::new("")),
            ..RemoteCredentials::default()
        };
        assert!(!empty_secret.has_s3_key_pair());

        let both = RemoteCredentials {
            access_key: Some(Secret::new("AKIA")),
            secret_key: Some(Secret::new("wJalr")),
            ..RemoteCredentials::default()
        };
        assert!(both.has_s3_key_pair());
    }

    #[test]
    fn nothing_stored_is_the_default() {
        let credentials = RemoteCredentials::default();
        assert!(!credentials.has_s3_key_pair());
        assert!(credentials.password.is_none());
        assert!(credentials.key_path.is_none());
    }
}
