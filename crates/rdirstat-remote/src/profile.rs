//! Presets, so that adding a destination is picking a service rather than
//! knowing its endpoint URL.
//!
//! The idea is Cyberduck's — a *connection profile* naming a vendor, its
//! endpoint template and which fields the user still has to fill in — and the
//! idea is all that is borrowed. Cyberduck is GPL-3.0 and this workspace is MIT
//! with a `deny.toml` that does not allow GPL, so nothing here is derived from
//! its code or from a `.cyberduckprofile`: the shape below is smaller, carries
//! no icons, and is written against the fields OpenDAL actually takes.
//!
//! The list is short on purpose. A profile earns its place by removing a
//! *specific* thing the user would otherwise have to look up — B2's endpoint
//! host, R2's mandatory `auto` region, Nextcloud's `/remote.php/dav/files/`
//! path — not by existing.

use crate::target::{RemoteKind, RemoteTarget};

/// A field the user must still supply, and what to call it on screen.
///
/// Which fields these are is the whole value of a profile: R2 asks for an
/// account id where AWS asks for a region, and a dialog that shows both to
/// both is the dialog people abandon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// An S3 bucket, a WebDAV collection, or an SFTP directory.
    Bucket,
    /// Where in that container this target is confined to.
    Root,
    /// The host, when the profile cannot know it (self-hosted, or per-account).
    Endpoint,
    Region,
    User,
    /// S3 access key and secret, or a WebDAV password.
    Secret,
}

/// A named service preset.
///
/// `Serialize` but deliberately not `Deserialize`: the profile list is a
/// compile-time constant that travels to the UI and never comes back. Making it
/// round-trip would mean owning every string, for a table nothing parses.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, specta::Type)]
pub struct Profile {
    /// Stable identifier. Persisted with a target so the UI can show which
    /// preset it came from; never shown raw.
    pub id: &'static str,
    pub label: &'static str,
    pub kind: RemoteKind,
    /// One line saying what this is, for a user who does not recognise the name.
    pub summary: &'static str,
    /// Endpoint, with `{}` where a user-supplied fragment goes — an account id
    /// for R2, a host for a self-hosted service. Empty means the backend's own
    /// default resolves it.
    pub endpoint_template: &'static str,
    /// Pinned when the service has exactly one valid answer.
    pub region: &'static str,
    /// What the user must still fill in, in the order it should be asked.
    pub required: &'static [Field],
}

impl Profile {
    /// Builds the endpoint for this profile from the user's fragment.
    #[must_use]
    pub fn endpoint_for(&self, fragment: &str) -> String {
        if self.endpoint_template.contains("{}") {
            self.endpoint_template.replace("{}", fragment.trim())
        } else if self.endpoint_template.is_empty() {
            fragment.trim().to_owned()
        } else {
            self.endpoint_template.to_owned()
        }
    }

    /// A target pre-filled from this profile, ready for the fields in
    /// [`Profile::required`].
    #[must_use]
    pub fn blank_target(&self, name: &str) -> RemoteTarget {
        RemoteTarget {
            name: name.to_owned(),
            kind: self.kind,
            endpoint: if self.endpoint_template.contains("{}") {
                String::new()
            } else {
                self.endpoint_template.to_owned()
            },
            bucket: String::new(),
            region: self.region.to_owned(),
            root: "/".to_owned(),
            user: String::new(),
        }
    }
}

/// Every preset this app ships.
pub const PROFILES: &[Profile] = &[
    Profile {
        id: "s3-aws",
        label: "Amazon S3",
        kind: RemoteKind::S3,
        summary: "Amazon's own S3. Uses your AWS profile or SSO session if you have one.",
        // Empty, so the SDK resolves the regional host itself. Pinning
        // s3.amazonaws.com would force every request through us-east-1 and
        // redirect, which costs a round trip per operation.
        endpoint_template: "",
        region: "",
        required: &[Field::Bucket, Field::Region, Field::Root],
    },
    Profile {
        id: "s3-r2",
        label: "Cloudflare R2",
        kind: RemoteKind::S3,
        summary: "S3-compatible with no egress fee. Needs your account ID, not a region.",
        endpoint_template: "https://{}.r2.cloudflarestorage.com",
        // R2 has no regions and rejects anything else, but SigV4 still signs
        // one, so it must be present and it must be this.
        region: "auto",
        required: &[Field::Endpoint, Field::Bucket, Field::Root, Field::Secret],
    },
    Profile {
        id: "s3-b2",
        label: "Backblaze B2",
        kind: RemoteKind::S3,
        summary: "Cheap archival storage through B2's S3-compatible endpoint.",
        endpoint_template: "https://s3.{}.backblazeb2.com",
        region: "",
        required: &[Field::Endpoint, Field::Bucket, Field::Root, Field::Secret],
    },
    Profile {
        id: "s3-wasabi",
        label: "Wasabi",
        kind: RemoteKind::S3,
        summary: "S3-compatible storage with no egress or request charges.",
        endpoint_template: "https://s3.{}.wasabisys.com",
        region: "",
        required: &[Field::Endpoint, Field::Bucket, Field::Root, Field::Secret],
    },
    Profile {
        id: "s3-minio",
        label: "MinIO or other S3-compatible",
        kind: RemoteKind::S3,
        summary: "Any S3 API on a host you run — MinIO, Ceph, SeaweedFS, a NAS.",
        endpoint_template: "",
        // Self-hosted gateways almost universally ignore the region but still
        // require the signature to name one.
        region: "us-east-1",
        required: &[Field::Endpoint, Field::Bucket, Field::Root, Field::Secret],
    },
    Profile {
        id: "dav-nextcloud",
        label: "Nextcloud or ownCloud",
        kind: RemoteKind::WebDav,
        summary: "Your Nextcloud's files over WebDAV. Use an app password, not your login.",
        // The path is the part nobody remembers, and getting it wrong returns
        // a 404 that looks like the server is down.
        endpoint_template: "https://{}/remote.php/dav/files",
        region: "",
        required: &[Field::Endpoint, Field::User, Field::Secret, Field::Root],
    },
    Profile {
        id: "dav-generic",
        label: "WebDAV",
        kind: RemoteKind::WebDav,
        summary: "Any WebDAV collection — a Synology, an Apache with mod_dav, a Box account.",
        endpoint_template: "",
        region: "",
        required: &[Field::Endpoint, Field::User, Field::Secret, Field::Root],
    },
    Profile {
        id: "sftp",
        label: "SFTP or SCP",
        kind: RemoteKind::Sftp,
        summary: "A server you already reach over SSH. Uses your existing keys and ssh-agent.",
        endpoint_template: "",
        region: "",
        // No Secret: this is the whole point of the SFTP backend. If the user
        // can `ssh` there, this works, and nothing is stored.
        required: &[Field::Endpoint, Field::User, Field::Root],
    },
];

/// Looks a profile up by its stable id.
#[must_use]
pub fn profile(id: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|profile| profile.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_id_is_unique() {
        let mut ids: Vec<&str> = PROFILES.iter().map(|profile| profile.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "two profiles share an id");
    }

    #[test]
    fn a_template_substitutes_the_users_fragment() {
        let r2 = profile("s3-r2").expect("the R2 profile ships with this crate");
        assert_eq!(r2.endpoint_for("abc123"), "https://abc123.r2.cloudflarestorage.com");
    }

    #[test]
    fn a_profile_with_no_template_takes_the_host_verbatim() {
        let minio = profile("s3-minio").expect("the MinIO profile ships with this crate");
        assert_eq!(minio.endpoint_for("minio.lan:9000"), "minio.lan:9000");
    }

    // R2 rejects every region but `auto`, and a signature has to name one, so
    // this being right is the difference between working and a 403.
    #[test]
    fn r2_pins_the_only_region_it_accepts() {
        assert_eq!(
            profile("s3-r2").expect("the R2 profile ships with this crate").region,
            "auto"
        );
    }

    #[test]
    fn sftp_asks_for_no_secret() {
        let sftp = profile("sftp").expect("the SFTP profile ships with this crate");
        assert!(!sftp.required.contains(&Field::Secret));
        assert!(!sftp.kind.needs_stored_secret());
    }

    #[test]
    fn every_s3_profile_asks_for_a_bucket() {
        for preset in PROFILES.iter().filter(|profile| profile.kind == RemoteKind::S3) {
            assert!(preset.required.contains(&Field::Bucket), "{}", preset.id);
        }
    }

    // A blank target is what the editor opens with, so it must already survive
    // validation once the required fields are filled — otherwise the dialog
    // shows an error before the user has typed anything.
    #[test]
    fn a_filled_blank_target_validates() {
        let preset = profile("s3-minio").expect("the MinIO profile ships with this crate");
        let mut target = preset.blank_target("nas");
        target.endpoint = preset.endpoint_for("minio.lan:9000");
        target.bucket = "backups".to_owned();
        let clean = target.validated().expect("a filled profile target should validate");
        assert_eq!(clean.region, "us-east-1");
        assert_eq!(clean.root, "/");
    }
}
