//! Directory readers, the two schedulers, the single-writer builder, and the
//! traversal policy that makes their results comparable.
//!
//! No Tauri dependency: this crate is driven by the CLI, tests, and benchmarks,
//! and by a thin desktop adapter that adds nothing to it.
//!
//! # Shape
//!
//! ```text
//!   DirReader  ──batches──▶  ScanBuilder  ──rollup──▶  Tree
//!      ▲                          ▲
//!      │                          │
//!   scheduler (sequential | bounded parallel)
//! ```
//!
//! Reading one directory and scheduling the traversal are separate concerns.
//! Both schedulers drive the same [`DirReader`] and hand every batch to the
//! same builder, so the only thing they can disagree about is order — which is
//! what the differential test in `tests/differential.rs` pins down.
//!
//! # What is decided where
//!
//! * **[`StdReader`]** — the correctness oracle. `std::fs::read_dir` plus
//!   `DirEntry::metadata`, which does not follow symlinks on Unix. Expected to
//!   be the slow baseline; no faster reader may disagree with it silently.
//! * **[`Engine`]** — sequential (one explicit stack) or bounded parallel
//!   (fixed worker pool, bounded `crossbeam` channels, an **exact**
//!   pending-directory termination counter). The worker count is explicit and
//!   is recorded in the result, because `num_cpus` on a sequential disk
//!   workload is seek contention, not throughput.
//! * **The builder** — the only assigner of [`NodeId`](rdirstat_core::NodeId)s
//!   and the only place traversal policy lives.
//!
//! # Traversal policy
//!
//! Every rule below is from docs/02-SCANNER.md#traversal-rules and has a test:
//!
//! * symlinks are never followed and are always leaves;
//! * device boundaries are not crossed unless `cross_filesystems` is set; a
//!   mount marker is retained either way;
//! * Darwin firmlinks ([`SF_FIRMLINK`]) are never descended;
//! * hard-linked content is counted once per `(dev, ino)`; later entries stay
//!   visible, carry [`flags::HARD_LINK_REPEAT`](rdirstat_core::flags::HARD_LINK_REPEAT),
//!   and contribute zero bytes;
//! * logical and allocated bytes stay separate and are never summed;
//! * a directory's own files are a *virtual* `<Files>` group — no arena node;
//! * sockets, FIFOs, and devices are zero-contribution leaves whose contents
//!   are never opened;
//! * `EACCES`/`EPERM` mark a node unreadable and the scan **continues**. A
//!   per-directory failure never leaves the scan as a `Result::Err`.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use rdirstat_scan::{Engine, ScanOutcome, Scanner};
//!
//! let scanner = Scanner::new().with_engine(Engine::parallel(4));
//! let cancel = scanner.cancel_token(); // hand this to the UI thread
//! match scanner.scan(Path::new("/Users/me/Downloads"))? {
//!     ScanOutcome::Completed(scan) => println!("{} nodes", scan.tree.len()),
//!     _ => println!("cancelled"),
//! }
//! # Ok::<(), rdirstat_scan::ScanFailure>(())
//! ```
//!
//! # `unsafe`
//!
//! There is none. The workspace lowers `unsafe_code` to `deny` (not `forbid`)
//! only so the future `getattrlistbulk(2)` reader can lower it locally in one
//! audited module; nothing in this crate as shipped uses it.

mod builder;
mod cancel;
mod categorize;
mod engine;
mod entry;
mod exclude;
mod hardlink;
mod progress;
mod reader;
mod scanner;
mod std_reader;

pub use crate::cancel::{CANCEL_CHECK_ENTRIES, CancelToken};
pub use crate::categorize::{Categorizer, PACKAGE_SUFFIXES, Uncategorized, is_package_name};
pub use crate::engine::Engine;
pub use crate::engine::parallel::CHUNK_ENTRIES;
pub use crate::entry::{EntryError, INLINE_NAME_BYTES, RawEntry, RawEntryBatch, SmallName};
pub use crate::exclude::{ExclusionSet, Verdict, default_exclusions, match_path, match_segment, path_rule};
pub use crate::hardlink::HardLinkSet;
pub use crate::progress::{
    ArenaFootprint, Counters, CurrentDir, NoProgress, PROGRESS_INTERVAL, ProgressPublisher, ProgressSink,
};
pub use crate::reader::{DirHandle, DirReader, ReadDirError, SF_FIRMLINK, classify_os_error};
pub use crate::scanner::{ScanFailure, ScanOutcome, Scanner, engine_for, validate_options};
pub use crate::std_reader::{BROKEN_SYMLINK_PROBE, DEFAULT_BATCH_ENTRIES, DEFAULT_BATCH_NAME_BYTES, StdReader};
