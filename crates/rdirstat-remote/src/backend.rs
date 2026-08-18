//! Turning a saved target plus a resolved secret into something that transfers.
//!
//! One type, [`Remote`], wrapping one `opendal::Operator`. There is no
//! per-protocol trait and no `Box<dyn Backend>`, because after the operator is
//! built there is nothing left for a trait to dispatch on: OpenDAL already
//! erased the protocol. A trait here would be an abstraction over an
//! abstraction, and the only thing it would add is a second place for the three
//! backends to drift apart.
//!
//! What *is* per-protocol lives in [`connect`] — the builder wiring — and in
//! [`Remote::comparison`], which reports what evidence this particular
//! endpoint can offer that a file is already there.

use std::time::Duration;

use futures_util::StreamExt as _;
use opendal::layers::{ConcurrentLimitLayer, RetryLayer, TimeoutLayer};
use opendal::{Operator, services};

use crate::credentials::RemoteCredentials;
use crate::target::{RemoteKind, RemoteTarget, TargetError};

/// Ceiling on how many remote entries a single listing will materialise.
///
/// The same reasoning as `sync::MAX_PLANNED_ENTRIES`, for a different cost: a
/// bucket is not bounded by a disk, and `ListObjectsV2` will page through ten
/// million keys as fast as it is asked to. Past this the plan reports that it
/// stopped rather than pretending the tail is absent — which, for a *missing
/// files* comparison, would mean re-uploading everything past the cap.
pub const MAX_LISTED_ENTRIES: usize = 500_000;

/// How long a single operation may hang before it is a failure rather than a
/// slow link.
///
/// Generous, because the alternative failure is worse: a 40-minute multipart
/// upload aborted at 39 minutes by a timeout meant for a `stat`. `TimeoutLayer`
/// distinguishes the two — `with_timeout` bounds a whole non-IO operation,
/// `with_io_timeout` bounds the gap between bytes, so a transfer that is still
/// moving is never cut off no matter how long it runs.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
const IO_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent requests allowed per target.
///
/// Not a throughput knob — a politeness one. A self-hosted Nextcloud or a
/// home NAS is the common destination here, and the failure mode of an
/// unbounded pool against one is connection refused for everything, including
/// the user's browser.
const CONCURRENCY: usize = 8;

/// What a target could not do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RemoteError {
    #[error("{0}")]
    Target(#[from] TargetError),
    /// The endpoint refused the credentials, or none were resolvable.
    #[error("{0} did not accept these credentials: {1}")]
    Unauthorized(String, String),
    /// The endpoint could not be reached at all.
    #[error("{0} could not be reached: {1}")]
    Unreachable(String, String),
    /// Reached and authorised, but the operation failed.
    #[error("{operation} failed on {path}: {reason}")]
    Operation {
        operation: &'static str,
        path: String,
        reason: String,
    },
}

impl RemoteError {
    /// Classifies an OpenDAL error into the three things a user can act on:
    /// fix your credentials, fix your network, or read what happened.
    ///
    /// Worth doing rather than surfacing the raw error because OpenDAL's own
    /// message for a bad access key is a 403 with an XML body, and the useful
    /// half of that — *your key is wrong* — is not the half that renders.
    fn classify(display: &str, operation: &'static str, path: &str, error: &opendal::Error) -> Self {
        let reason = error.to_string();
        match error.kind() {
            opendal::ErrorKind::PermissionDenied | opendal::ErrorKind::ConfigInvalid => {
                Self::Unauthorized(display.to_owned(), reason)
            }
            opendal::ErrorKind::Unexpected if error.is_temporary() => Self::Unreachable(display.to_owned(), reason),
            _ => Self::Operation {
                operation,
                path: path.to_owned(),
                reason,
            },
        }
    }
}

/// What a backend can prove about a file it already holds.
///
/// This is the remote answer to `sync::CompareMode`, and it is not a user
/// setting because it is not a choice: it is a fact about the endpoint. A local
/// sync can always fall back to reading both sides. A remote one cannot —
/// "compare the contents" against S3 means downloading the object, which costs
/// egress and takes longer than re-uploading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// Size only. The honest answer for a backend that returns no digest, and
    /// for S3 objects whose ETag is a multipart digest (see
    /// [`usable_etag`]).
    Size,
    /// Size, and an MD5 the endpoint computed itself. Catches a file edited in
    /// place without changing length — the case size alone is blind to.
    SizeAndDigest,
}

/// One remote object, as a listing reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Path relative to the target's root, with `/` separators and no leading
    /// slash — the same shape as `sync::SyncEntry::relative_path`, so the two
    /// planners compare like with like.
    pub relative_path: String,
    pub bytes: u64,
    /// A content digest the endpoint vouches for, when it offers one that is
    /// actually comparable to a local hash.
    pub digest: Option<String>,
}

/// An open connection to one target.
#[derive(Clone, Debug)]
pub struct Remote {
    operator: Operator,
    display: String,
    kind: RemoteKind,
}

/// Builds an operator for a target.
///
/// Validates the target first, so a malformed one fails here with a field name
/// rather than inside OpenDAL with a config error.
///
/// # Errors
///
/// [`RemoteError::Target`] for a bad field, [`RemoteError::Unauthorized`] when
/// the backend rejects the configuration outright.
pub fn connect(target: &RemoteTarget, credentials: &RemoteCredentials) -> Result<Remote, RemoteError> {
    let target = target.validated()?;
    let display = target.display();

    let operator = match target.kind {
        RemoteKind::S3 => connect_s3(&target, credentials),
        RemoteKind::WebDav => connect_webdav(&target, credentials),
        RemoteKind::Sftp => connect_sftp(&target, credentials),
    }
    .map_err(|error| RemoteError::classify(&display, "connect", &target.root, &error))?;

    // Order matters and is not cosmetic. Layers wrap outward-in, so retry sits
    // OUTSIDE timeout: a request that stalls is cut by the timeout and then
    // retried, which is the behaviour wanted. The reverse — timeout outside
    // retry — would give the whole retry sequence one shared deadline, so the
    // second attempt inherits whatever the first one left of it.
    let operator = operator
        .layer(ConcurrentLimitLayer::new(CONCURRENCY))
        .layer(
            RetryLayer::new()
                .with_max_times(4)
                // Jitter, because every file in a 100k-file transfer that hits
                // the same throttle would otherwise retry in lockstep and
                // reproduce the burst that caused it.
                .with_jitter()
                .with_min_delay(Duration::from_millis(200))
                .with_max_delay(Duration::from_secs(20)),
        )
        .layer(
            TimeoutLayer::new()
                .with_timeout(OPERATION_TIMEOUT)
                .with_io_timeout(IO_STALL_TIMEOUT),
        );

    Ok(Remote {
        operator,
        display,
        kind: target.kind,
    })
}

fn connect_s3(target: &RemoteTarget, credentials: &RemoteCredentials) -> Result<Operator, opendal::Error> {
    let mut builder = services::S3::default().bucket(&target.bucket).root(&target.root);

    if !target.region.is_empty() {
        builder = builder.region(&target.region);
    }
    if !target.endpoint.is_empty() {
        builder = builder.endpoint(&with_scheme(&target.endpoint));
    }

    if credentials.has_s3_key_pair() {
        // An explicit key was stored for this target, so it wins outright and
        // the ambient chain is switched OFF. Leaving it on would make the
        // effective identity depend on whether the user happened to have
        // AWS_PROFILE exported — which is how a backup silently starts landing
        // in the wrong account.
        builder = builder.disable_config_load();
        if let Some(access) = &credentials.access_key {
            builder = builder.access_key_id(access.expose());
        }
        if let Some(secret) = &credentials.secret_key {
            builder = builder.secret_access_key(secret.expose());
        }
        if let Some(token) = &credentials.session_token {
            builder = builder.session_token(token.expose());
        }
    }
    // Otherwise config load stays ON, which is what resolves AWS_* from the
    // environment, then ~/.aws/credentials and an SSO session, then an
    // instance role — steps 2 through 4 of the order this app promises.

    Operator::new(builder)
}

fn connect_webdav(target: &RemoteTarget, credentials: &RemoteCredentials) -> Result<Operator, opendal::Error> {
    let mut builder = services::Webdav::default()
        .endpoint(&with_scheme(&target.endpoint))
        .root(&target.root);

    if !target.user.is_empty() {
        builder = builder.username(&target.user);
    }
    if let Some(password) = &credentials.password {
        builder = builder.password(password.expose());
    }

    Operator::new(builder)
}

fn connect_sftp(target: &RemoteTarget, credentials: &RemoteCredentials) -> Result<Operator, opendal::Error> {
    let mut builder = services::Sftp::default().endpoint(&target.endpoint).root(&target.root);

    if !target.user.is_empty() {
        builder = builder.user(&target.user);
    }
    if let Some(key_path) = &credentials.key_path {
        builder = builder.key(key_path);
    }
    // `known_hosts_strategy` is deliberately left at its default of Strict, and
    // this app offers no setting to relax it. "Accept" makes the first
    // connection to any host succeed, which is precisely the connection a
    // man-in-the-middle needs to win. A user who genuinely needs to trust a new
    // host can run `ssh host` once — the same file, the same prompt, and a
    // decision made by ssh rather than silently by this app.

    Operator::new(builder)
}

/// Gives a host a scheme if the user typed it without one.
///
/// `minio.lan:9000` is what people type and is not a URL; OpenDAL wants
/// `http://minio.lan:9000`. Defaulting to **https** rather than http is the
/// safe direction: a plain-http endpoint fails loudly and the user adds the
/// scheme, whereas defaulting to http would silently send an S3 access key
/// across the network in the clear.
fn with_scheme(endpoint: &str) -> String {
    if endpoint.contains("://") {
        endpoint.to_owned()
    } else {
        format!("https://{endpoint}")
    }
}

/// True when an S3 ETag can be compared to a locally computed MD5.
///
/// **This is the trap the whole remote comparison rests on.** An S3 ETag is the
/// MD5 of the object *only* when it was uploaded in one part and not
/// server-side encrypted with KMS. For a multipart upload the ETag is the MD5
/// of the concatenated part digests followed by `-<part count>` — a different
/// number entirely, and one that depends on the part size the *uploader* chose.
///
/// Comparing a local MD5 to a multipart ETag therefore reports "differs" for
/// every large file that is in fact identical, and a sync that re-uploads every
/// file over the multipart threshold on every run is worse than no comparison
/// at all. The `-` is the tell, and the only reliable one.
///
/// (`reference-code/rustfs/crates/checksums` is the same rule from the server
/// side: it keeps the streaming-hash registry separate from the composite one.)
#[must_use]
pub fn usable_etag(etag: Option<&str>) -> Option<String> {
    let etag = etag?.trim_matches('"');
    if etag.contains('-') || etag.len() != 32 || !etag.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(etag.to_ascii_lowercase())
}

impl Remote {
    /// The target's human-readable address. Never contains a credential.
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub fn kind(&self) -> RemoteKind {
        self.kind
    }

    /// What evidence this endpoint can give that a file is already present.
    ///
    /// Decided by protocol, because only one of the three publishes a digest of
    /// the *content*:
    ///
    /// - **S3** returns an ETag, which is an MD5 for single-part objects. The
    ///   multipart case is filtered out per object by [`usable_etag`], so this
    ///   claims only that a digest is *sometimes* available — which is why
    ///   `RemoteEntry::digest` is an `Option` rather than this being the whole
    ///   answer.
    /// - **WebDAV** also has a `getetag`, and it is *not* usable. RFC 4918
    ///   defines it as an opaque validator, and real servers derive it from
    ///   mtime and inode — Nextcloud's changes when a file is touched and does
    ///   not change when a file is edited in place at the same length. Treating
    ///   it as a content hash would be worse than having none, because it looks
    ///   like verification while detecting nothing.
    /// - **SFTP** publishes no digest at all. The protocol has no such request.
    ///
    /// OpenDAL's `Capability` carries no flag for this in 0.58, so there is no
    /// probe to prefer over the protocol.
    #[must_use]
    pub fn comparison(&self) -> Comparison {
        match self.kind {
            RemoteKind::S3 => Comparison::SizeAndDigest,
            RemoteKind::WebDav | RemoteKind::Sftp => Comparison::Size,
        }
    }

    /// Confirms the endpoint is reachable and the credentials work.
    ///
    /// Deliberately not a `list`: a bucket the user can write but not list is
    /// an ordinary least-privilege setup, and probing with a list would report
    /// a working target as broken. `Operator::check` performs the cheapest
    /// operation the backend supports for exactly this purpose.
    ///
    /// # Errors
    ///
    /// [`RemoteError::Unauthorized`] or [`RemoteError::Unreachable`].
    pub async fn probe(&self) -> Result<(), RemoteError> {
        self.operator
            .check()
            .await
            .map_err(|error| RemoteError::classify(&self.display, "connect", "/", &error))
    }

    /// Every object under the target's root, keyed by path relative to it.
    ///
    /// Streams rather than collecting, and stops at [`MAX_LISTED_ENTRIES`],
    /// returning `truncated = true` so the caller can say so instead of
    /// treating the missing tail as absent files.
    ///
    /// # Errors
    ///
    /// [`RemoteError`] if the listing could not be started or a page failed.
    pub async fn list(&self) -> Result<(Vec<RemoteEntry>, bool), RemoteError> {
        let mut lister = self
            .operator
            .lister_with("")
            .recursive(true)
            .await
            .map_err(|error| RemoteError::classify(&self.display, "list", "/", &error))?;

        let mut entries = Vec::new();
        let mut truncated = false;
        while let Some(item) = lister.next().await {
            let entry = item.map_err(|error| RemoteError::classify(&self.display, "list", "/", &error))?;
            let metadata = entry.metadata();
            // A recursive listing still names the directories between objects.
            // They carry no bytes and have no local counterpart to compare, so
            // counting them would inflate the "already present" tally.
            if !metadata.is_file() {
                continue;
            }
            if entries.len() >= MAX_LISTED_ENTRIES {
                truncated = true;
                break;
            }
            entries.push(RemoteEntry {
                relative_path: entry.path().trim_start_matches('/').to_owned(),
                bytes: metadata.content_length(),
                // Gated on the protocol, not just on the header being
                // present: a WebDAV server happily returns a `getetag` that is
                // an mtime token, and letting it through here would make
                // `Verify` report every file as changed after a `touch`.
                digest: match self.kind {
                    RemoteKind::S3 => metadata
                        .content_md5()
                        .map(str::to_owned)
                        .or_else(|| usable_etag(metadata.etag())),
                    RemoteKind::WebDav | RemoteKind::Sftp => None,
                },
            });
        }
        Ok((entries, truncated))
    }

    /// The operator, for the transfer manager's own read and write paths.
    ///
    /// Exposed rather than wrapped because a transfer needs `writer_with` and
    /// its chunking options, and re-exporting that surface one method at a time
    /// would be a worse abstraction than the one OpenDAL already ships.
    #[must_use]
    pub fn operator(&self) -> &Operator {
        &self.operator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3_target() -> RemoteTarget {
        RemoteTarget {
            name: "backup".to_owned(),
            kind: RemoteKind::S3,
            endpoint: String::new(),
            bucket: "photos".to_owned(),
            region: "us-west-2".to_owned(),
            root: "archive".to_owned(),
            user: String::new(),
        }
    }

    #[test]
    fn a_bare_host_becomes_https_not_http() {
        assert_eq!(with_scheme("minio.lan:9000"), "https://minio.lan:9000");
        assert_eq!(with_scheme("http://minio.lan:9000"), "http://minio.lan:9000");
        assert_eq!(with_scheme("https://s3.example.com"), "https://s3.example.com");
    }

    // The single most consequential rule in this file. A multipart ETag is not
    // an MD5, and treating it as one re-uploads every large file forever.
    #[test]
    fn a_multipart_etag_is_not_a_digest() {
        assert_eq!(usable_etag(Some("d41d8cd98f00b204e9800998ecf8427e-4")), None);
        assert_eq!(usable_etag(Some("\"d41d8cd98f00b204e9800998ecf8427e-137\"")), None);
    }

    #[test]
    fn a_single_part_etag_is_a_digest() {
        assert_eq!(
            usable_etag(Some("\"D41D8CD98F00B204E9800998ECF8427E\"")).as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
    }

    #[test]
    fn a_non_md5_etag_is_rejected() {
        assert_eq!(usable_etag(None), None);
        assert_eq!(usable_etag(Some("")), None);
        assert_eq!(usable_etag(Some("not-a-digest")), None);
        // Right length, wrong alphabet: some gateways return an opaque token.
        assert_eq!(usable_etag(Some("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")), None);
    }

    // Building an operator must not touch the network, or every one of these
    // is a flake and the whole crate becomes untestable offline.
    #[test]
    fn every_kind_builds_without_a_network() {
        let credentials = RemoteCredentials::default();
        assert!(connect(&s3_target(), &credentials).is_ok());

        let webdav = RemoteTarget {
            kind: RemoteKind::WebDav,
            endpoint: "nextcloud.lan/remote.php/dav/files/josh".to_owned(),
            bucket: String::new(),
            user: "josh".to_owned(),
            ..s3_target()
        };
        assert!(connect(&webdav, &credentials).is_ok());

        let sftp = RemoteTarget {
            kind: RemoteKind::Sftp,
            endpoint: "nas.lan".to_owned(),
            bucket: String::new(),
            user: "josh".to_owned(),
            ..s3_target()
        };
        assert!(connect(&sftp, &credentials).is_ok());
    }

    #[test]
    fn a_bad_target_fails_with_its_field_name_not_a_config_error() {
        let mut target = s3_target();
        target.bucket = String::new();
        let error = connect(&target, &RemoteCredentials::default()).expect_err("a bucketless S3 target cannot connect");
        assert!(
            matches!(error, RemoteError::Target(TargetError::Bucket(_))),
            "{error:?}"
        );
    }

    #[test]
    fn the_display_address_carries_no_credential() {
        let credentials = RemoteCredentials {
            access_key: Some(crate::credentials::Secret::new("AKIAIOSFODNN7EXAMPLE")),
            secret_key: Some(crate::credentials::Secret::new("wJalrXUtnFEMI")),
            ..RemoteCredentials::default()
        };
        let remote = connect(&s3_target(), &credentials).expect("a complete S3 target should build an operator");
        assert_eq!(remote.display(), "s3://photos/archive/");
        assert!(!remote.display().contains("AKIA"));
    }
}
