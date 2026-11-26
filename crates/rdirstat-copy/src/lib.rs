//! The one copier: ditto, streaming compare, SHA-256, APFS clonefile, and identity re-stat. No Tauri.
//!
//! Every byte this application copies goes through this crate
//! (`.settings/docs/11-ORGANIZE.md`, Amendment A, "One copier"). Relocate,
//! Sync and Backup call it; none of them spawns `ditto` or compares files on
//! its own. [`land`] is the one-item entry point Backup uses: clone a known
//! duplicate or `ditto`, then verify.
//!
//! # What this crate never does
//!
//! It never writes to, renames, removes or replaces a *source* path. Sources
//! are opened read-only (`File::open`) or passed to `ditto` / `clonefile(2)` as
//! the side being read. It never deletes a destination either, partial or
//! not: a failed copy is reported and left on disk for the caller (and the
//! user) to inspect.
//!
//! # Unsafe
//!
//! The workspace denies `unsafe_code`. This crate owns four tiny audited FFI
//! calls — `clonefile(2)`, `listxattr(2)`, `statfs(2)` and `pathconf(2)` —
//! each in its own function with `#[allow(unsafe_code)]` and a `SAFETY` note.

use std::io;
use std::path::{Path, PathBuf};

mod clone;
mod compare;
mod disk;
mod ditto;
mod hash;
mod identity;
mod land;

pub use clone::clone_file;
pub use compare::{
    COMPARE_CHUNK, VerifyTally, compare_contents_tree, compare_files, compare_tree, compare_xattr_names,
    contents_differ,
};
pub use disk::{DiskIdentity, disk_identity};
pub use ditto::{ditto, ditto_bom};
pub use hash::{sha256_file, sha256_hex};
pub use identity::{Identity, identity};
pub use land::{Landed, Method, land, partial_path};

/// Everything that can go wrong copying, comparing, cloning or identifying.
///
/// The `Display` strings for [`CopyError::Spawn`] and [`CopyError::Tool`] are
/// `could not run ditto: …` and `ditto exited N: …`; Relocate and Sync surface
/// them verbatim, so they are part of the user-visible behaviour.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CopyError {
    /// A helper program (`ditto`, `diskutil`) could not be started.
    #[error("could not run {program}: {source}")]
    Spawn {
        program: &'static str,
        #[source]
        source: io::Error,
    },

    /// A helper program ran and exited non-zero (`code` is -1 for a signal).
    #[error("{program} exited {code}: {stderr}")]
    Tool {
        program: &'static str,
        code: i32,
        stderr: String,
    },

    /// Both sides were readable and they are not the same. `path` is the side
    /// the difference was found on (normally the destination).
    #[error("{}: {reason}", path.display())]
    Mismatch { path: PathBuf, reason: String },

    /// A filesystem call on `path` failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// [`clone_file`] refuses to write over anything that already exists.
    #[error("{} already exists; refusing to replace it", path.display())]
    AlreadyExists { path: PathBuf },

    /// `clonefile(2)` is not available for this pair: the filesystem does not
    /// support clones (`ENOTSUP`) or the two paths are on different volumes
    /// (`EXDEV`). The caller falls back to a real copy.
    #[error("cannot clone onto {}: {source}", path.display())]
    CloneUnsupported {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The caller asked to stop before anything was written.
    #[error("cancelled")]
    Cancelled,

    /// Something was written at `path` and the item did not land: the copy
    /// failed part-way, the verify failed, or the run was cancelled. `path` is
    /// left on disk, never deleted, so the user can inspect or remove it.
    #[error("{cause}; the partial copy is left at {}", path.display())]
    Partial {
        path: PathBuf,
        #[source]
        cause: Box<CopyError>,
    },

    /// A path could not be handed to the OS (it contains a NUL byte).
    #[error("{} cannot be passed to the operating system: it contains a NUL byte", path.display())]
    InvalidPath { path: PathBuf },
}

impl CopyError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn mismatch(path: &Path, reason: impl Into<String>) -> Self {
        Self::Mismatch {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }
}

/// A path as a NUL-terminated C string, for the FFI calls.
pub(crate) fn c_path(path: &Path) -> Result<std::ffi::CString, CopyError> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| CopyError::InvalidPath {
        path: path.to_path_buf(),
    })
}

/// Runs a helper program and turns a non-zero exit into [`CopyError::Tool`].
pub(crate) fn run(program: &'static str, command: &mut std::process::Command) -> Result<Vec<u8>, CopyError> {
    let output = command
        .output()
        .map_err(|source| CopyError::Spawn { program, source })?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(CopyError::Tool {
        program,
        code: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}
