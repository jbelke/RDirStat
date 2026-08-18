//! What a remote endpoint *is*, separated from what it takes to open one.
//!
//! A [`RemoteTarget`] is the part that gets written to disk: a name, a
//! protocol, a host, and where in that host's namespace this app is allowed to
//! write. It carries **no secret of any kind**, and that is a structural
//! property rather than a convention — there is nowhere in this struct to put
//! one. The password, the access key and the session token live in the macOS
//! Keychain and are joined to the target only at the moment a connection is
//! opened. A settings file that leaks is a list of hostnames.

use serde::{Deserialize, Serialize};

/// Which protocol a target speaks.
///
/// Deliberately three, not the thirty-odd OpenDAL can register. Each one here
/// is reachable from a stock macOS with no extra software, has a credential
/// story this app can actually honour, and answers a question the disk-usage
/// tool it hangs off asks: *where do I put the 400 GB I just found?*
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    /// Amazon S3 and everything that speaks its API — MinIO, Backblaze B2,
    /// Wasabi, Cloudflare R2, DigitalOcean Spaces, Ceph.
    S3,
    /// WebDAV, which in practice means Nextcloud, ownCloud, Box or a Synology.
    WebDav,
    /// SFTP over SSH. Also what "SCP" means to everyone who asks for SCP: the
    /// scp(1) protocol itself was deprecated by OpenSSH in favour of this, and
    /// `scp` on a modern macOS is already an SFTP client wearing scp's flags.
    Sftp,
}

impl RemoteKind {
    /// The URL scheme this kind is displayed and parsed as.
    #[must_use]
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::WebDav => "davs",
            Self::Sftp => "sftp",
        }
    }

    /// Whether a stored secret is expected for this kind.
    ///
    /// SFTP is `false` because it authenticates through the SSH agent and
    /// `~/.ssh/config` like every other ssh client on the machine. Prompting
    /// for a password this app would then have to store, when the user already
    /// has a working key, is how a tool ends up holding credentials it never
    /// needed.
    #[must_use]
    pub const fn needs_stored_secret(self) -> bool {
        match self {
            Self::S3 | Self::WebDav => true,
            Self::Sftp => false,
        }
    }
}

/// A field of a target that was not usable, and why.
///
/// Names the field, because "invalid target" in a dialog with eight inputs is
/// not an error message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type, thiserror::Error)]
#[serde(tag = "field", content = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TargetError {
    #[error("the name {0}")]
    Name(String),
    #[error("the endpoint {0}")]
    Endpoint(String),
    #[error("the bucket {0}")]
    Bucket(String),
    #[error("the folder {0}")]
    Root(String),
    #[error("the user name {0}")]
    User(String),
}

/// A saved remote endpoint. Persisted. Contains no credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct RemoteTarget {
    /// What the user calls it, and the Keychain account its secret is filed
    /// under. Unique across the target list.
    pub name: String,
    pub kind: RemoteKind,
    /// The host or service URL.
    ///
    /// For S3 this is the API endpoint (`s3.us-west-2.amazonaws.com`,
    /// `minio.lan:9000`) and may be empty, in which case the region picks the
    /// AWS default. For WebDAV it is the collection URL. For SFTP it is
    /// `host` or `host:port`.
    pub endpoint: String,
    /// S3 only. Empty for the other kinds.
    pub bucket: String,
    /// S3 only, and it is not optional in the way it looks: SigV4 signs the
    /// region, so a wrong one fails to authenticate rather than failing to
    /// route. Empty means "let the SDK's own resolution decide".
    pub region: String,
    /// The subtree this target is confined to. Everything this app reads or
    /// writes is under it, and a path that would escape it is rejected before
    /// a request is built.
    pub root: String,
    /// The account name, where the protocol has one. S3 does not — its
    /// identity is the access key.
    pub user: String,
}

impl RemoteTarget {
    /// Checks every field and normalises the ones with a canonical form.
    ///
    /// Returns the target it validated rather than mutating in place, so a
    /// caller cannot half-validate one and use it anyway.
    ///
    /// # Errors
    ///
    /// [`TargetError`], naming the offending field.
    pub fn validated(&self) -> Result<Self, TargetError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(TargetError::Name("cannot be empty".to_owned()));
        }
        // The name becomes a Keychain account and a job-file stem. A path
        // separator or a NUL in either is a way to write somewhere else.
        if name.contains(['/', '\\', '\0']) {
            return Err(TargetError::Name("cannot contain a slash or a null byte".to_owned()));
        }

        // S3 alone tolerates an empty endpoint, because the region resolves the
        // AWS host for it. WebDAV and SFTP have nothing to resolve from, so a
        // blank host is a request to connect to nowhere.
        let endpoint = self.endpoint.trim();
        if self.kind != RemoteKind::S3 && endpoint.is_empty() {
            return Err(TargetError::Endpoint("cannot be empty".to_owned()));
        }
        if endpoint.contains(char::is_whitespace) {
            return Err(TargetError::Endpoint("cannot contain spaces".to_owned()));
        }

        let bucket = self.bucket.trim();
        if self.kind == RemoteKind::S3 && bucket.is_empty() {
            return Err(TargetError::Bucket("is required for S3".to_owned()));
        }
        if bucket.contains(['/', '\0']) {
            return Err(TargetError::Bucket("cannot contain a slash".to_owned()));
        }

        let root =
            normalize_root(&self.root).ok_or_else(|| TargetError::Root("cannot contain a `..` segment".to_owned()))?;

        let user = self.user.trim();
        if user.contains(['\0', '\n']) {
            return Err(TargetError::User("cannot contain a null byte or a newline".to_owned()));
        }
        if self.kind == RemoteKind::WebDav && !self.user.is_empty() && user.is_empty() {
            return Err(TargetError::User("cannot be only spaces".to_owned()));
        }

        Ok(Self {
            name: name.to_owned(),
            kind: self.kind,
            endpoint: endpoint.to_owned(),
            bucket: bucket.to_owned(),
            region: self.region.trim().to_owned(),
            root,
            user: user.to_owned(),
        })
    }

    /// How the target reads in a list or a breadcrumb.
    ///
    /// Never includes a credential, because this string ends up in logs, in
    /// error messages and on screen.
    #[must_use]
    pub fn display(&self) -> String {
        match self.kind {
            RemoteKind::S3 => format!("s3://{}{}", self.bucket, self.root),
            RemoteKind::WebDav | RemoteKind::Sftp => {
                format!("{}://{}{}", self.kind.scheme(), self.endpoint, self.root)
            }
        }
    }
}

/// Puts a root into the one form the rest of the crate may assume: leading
/// slash, trailing slash, no `..`, no empty segments.
///
/// OpenDAL treats a root without a trailing slash as a *file* prefix, so
/// `/backup` and `/backup/` name different things and the difference is
/// silent — objects land at `/backupphotos/…`. Normalising once here is why no
/// call site has to remember that.
///
/// Returns `None` only for a `..`, which is the case that must not be papered
/// over: resolving it locally would let a saved target address a sibling of the
/// subtree the user confined it to.
fn normalize_root(raw: &str) -> Option<String> {
    let mut out = String::from("/");
    for segment in raw.split('/') {
        let segment = segment.trim();
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        out.push_str(segment);
        out.push('/');
    }
    Some(out)
}

/// Joins a relative path under a target's root, refusing anything that escapes.
///
/// The relative path arrives from a directory walk, so it is this crate's own
/// output — but it also arrives from the frontend on the apply path, and a
/// remote key assembled from webview input is exactly the string an attacker
/// would like to control. Checked on every call rather than trusted by
/// provenance.
///
/// # Errors
///
/// [`TargetError::Root`] when a segment is `..`, so a caller cannot get a key
/// that leaves the subtree.
pub fn join_key(root: &str, relative: &str) -> Result<String, TargetError> {
    let mut out = String::from(root);
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(TargetError::Root(format!(
                "`{relative}` would leave the target's folder"
            )));
        }
        out.push_str(segment);
        out.push('/');
    }
    // A key names an object, not a collection. The walk only ever passes files.
    if out.len() > root.len() {
        out.pop();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3() -> RemoteTarget {
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
    fn a_root_gains_both_slashes() {
        assert_eq!(normalize_root("archive").as_deref(), Some("/archive/"));
        assert_eq!(normalize_root("/archive").as_deref(), Some("/archive/"));
        assert_eq!(normalize_root("/archive/").as_deref(), Some("/archive/"));
        assert_eq!(normalize_root("").as_deref(), Some("/"));
        assert_eq!(normalize_root("///").as_deref(), Some("/"));
    }

    #[test]
    fn a_root_may_not_climb() {
        assert_eq!(normalize_root("../elsewhere"), None);
        assert_eq!(normalize_root("archive/../../etc"), None);
    }

    #[test]
    fn a_key_stays_under_its_root() {
        assert_eq!(
            join_key("/archive/", "a/b.txt").expect("a plain relative path is joinable"),
            "/archive/a/b.txt"
        );
        assert_eq!(
            join_key("/", "b.txt").expect("a plain relative path is joinable"),
            "/b.txt"
        );
        assert!(join_key("/archive/", "../escape.txt").is_err());
        assert!(join_key("/archive/", "a/../../escape.txt").is_err());
    }

    // `a//b` is not a path traversal, but a doubled separator produces an empty
    // S3 key segment, which some gateways normalise away and others store
    // literally. Collapsing it here means both behave the same.
    #[test]
    fn a_doubled_separator_collapses() {
        assert_eq!(
            join_key("/archive/", "a//b.txt").expect("a doubled separator is not an escape"),
            "/archive/a/b.txt"
        );
    }

    #[test]
    fn s3_requires_a_bucket() {
        let mut target = s3();
        target.bucket = String::new();
        assert!(matches!(target.validated(), Err(TargetError::Bucket(_))));
    }

    #[test]
    fn a_name_may_not_carry_a_separator() {
        let mut target = s3();
        target.name = "../../keychain".to_owned();
        assert!(matches!(target.validated(), Err(TargetError::Name(_))));
    }

    #[test]
    fn webdav_and_sftp_require_a_host() {
        for kind in [RemoteKind::WebDav, RemoteKind::Sftp] {
            let target = RemoteTarget {
                kind,
                endpoint: String::new(),
                bucket: String::new(),
                ..s3()
            };
            assert!(matches!(target.validated(), Err(TargetError::Endpoint(_))), "{kind:?}");
        }
    }

    #[test]
    fn validation_normalises_rather_than_only_checking() {
        let target = RemoteTarget {
            name: "  backup  ".to_owned(),
            root: "archive".to_owned(),
            ..s3()
        };
        let clean = target
            .validated()
            .expect("a target with every field set should validate");
        assert_eq!(clean.name, "backup");
        assert_eq!(clean.root, "/archive/");
    }

    #[test]
    fn only_sftp_authenticates_without_a_stored_secret() {
        assert!(!RemoteKind::Sftp.needs_stored_secret());
        assert!(RemoteKind::S3.needs_stored_secret());
        assert!(RemoteKind::WebDav.needs_stored_secret());
    }
}
