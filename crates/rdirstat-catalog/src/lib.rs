//! Parquet catalog: scan entries, content hashes, and backup receipts as
//! columnar files DuckDB can query in place.
//!
//! Three kinds of immutable part files, all zstd-compressed Parquet
//! (`select * from 'dir/hashes-*.parquet'`):
//!
//! - `entries-<run>-<seq>.parquet` — one row per retained node of a
//!   [`CompletedScan`](rdirstat_core::CompletedScan), written by
//!   [`write_entries`].
//! - `hashes-<run>-<seq>.parquet` — `(path, dev, ino, size, mtime, sha256)`,
//!   written by [`write_hashes`], read back by [`read_hashes`].
//! - `receipts-<run>-<seq>.parquet` — one row per verified landed copy or
//!   clone, written by [`write_receipts`], read back by [`read_receipts`].
//!
//! # Immutability
//!
//! A part file is never rewritten. Each write picks the next free `<seq>` for
//! its `(kind, run_id)`, writes a hidden temp file in the same directory,
//! fsyncs it, publishes it under its final name with `link(2)` — which fails
//! rather than replacing an existing file, so a race with another writer moves
//! on to the next `<seq>` — and fsyncs the directory. A crash can leave a
//! hidden `.…tmp-…` file behind; it never leaves a half-written file under a
//! name a reader picks up. Readers only open files whose names start with
//! `<kind>-` and end in `.parquet`.
//!
//! A fourth kind, `manifest-<run>-<seq>.parquet`, written by
//! [`Catalog::write_manifest`], lists every part a run wrote with its row
//! count, size, and SHA-256, so a mirror can be checked file by file.
//!
//! # Primary and mirror
//!
//! The free functions write to one directory. [`Catalog`] writes to the app
//! data dir and mirrors each published part, byte for byte and under the same
//! name, to `<dest>/.rdirstat/catalog/` ([`dest_catalog_dir`]) so a backup
//! drive describes itself. A mirror part is never overwritten.
//!
//! Catalog files hold filenames, which are sensitive: files are created mode
//! `0600`.
//!
//! This crate writes only inside the catalog directory it is handed. It never
//! opens, stats, or modifies the paths its rows describe.

mod catalog;
mod entries;
mod rows;
mod store;

use std::path::PathBuf;

pub use crate::catalog::{Catalog, DEST_CATALOG_DIR, ManifestRow, Written, dest_catalog_dir, read_manifests};
pub use crate::entries::write_entries;
pub use crate::rows::{HashRow, ReceiptRow, read_hashes, read_receipts, write_hashes, write_receipts};

/// Rows per Arrow record batch while writing `entries`.
///
/// Bounds writer memory on a tens-of-millions-node scan; a starting point
/// (docs/06-DATA.md "Write path"), not a tuned constant.
pub const ENTRIES_BATCH_ROWS: usize = 65_536;

/// Everything that can go wrong reading or writing a catalog part file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// A filesystem operation on a catalog path failed.
    #[error("{operation} {}: {source}", path.display())]
    Io {
        /// What was being attempted, e.g. `"create"`, `"fsync"`.
        operation: &'static str,
        /// The catalog path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The Parquet encoder or decoder failed.
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// Building an Arrow batch failed.
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    /// A run id that cannot be embedded in a file name.
    #[error("run id {0:?} must be 1-128 characters of [A-Za-z0-9._-] and start with a letter or digit")]
    InvalidRunId(String),
    /// A row that would write a value the schema forbids.
    #[error("row {index}: {detail}")]
    InvalidRow {
        /// Zero-based row index in the slice passed in.
        index: usize,
        /// What is wrong with it.
        detail: String,
    },
    /// A part file whose columns are not what this build writes.
    #[error("{}: {detail}", path.display())]
    Schema {
        /// The part file.
        path: PathBuf,
        /// The missing or mistyped column, or the null where none is allowed.
        detail: String,
    },
    /// The scan's arena could not be walked (a corrupt or cyclic tree).
    #[error("scan tree: {0}")]
    Tree(String),
    /// The mirror already holds a part of this name with different bytes.
    /// The mirror file is left as it is.
    #[error("mirror {} already exists with different contents", path.display())]
    MirrorConflict {
        /// The existing mirror file.
        path: PathBuf,
    },
    /// The part was written to the primary catalog, but mirroring it failed.
    /// The primary file stands; [`Catalog::sync_mirror`] retries the copy.
    #[error("written to {} but not mirrored: {source}", primary.display())]
    Mirror {
        /// The published primary part.
        primary: PathBuf,
        /// Why the mirror copy failed.
        #[source]
        source: Box<CatalogError>,
    },
}
