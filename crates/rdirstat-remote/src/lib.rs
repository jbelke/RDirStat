//! # `rdirstat-remote` — where the bytes go when they leave the disk
//!
//! The rest of this app answers *what is taking up 400 GB*. This crate answers
//! the question that immediately follows: **put it somewhere else.** It owns
//! the three remote destinations the app offers — S3, WebDAV and SFTP — and
//! nothing about how a transfer is scheduled, which is `src-tauri`'s
//! `transfers` module.
//!
//! ## Why one library and not three clients
//!
//! Every one of these is a byte store with a list, a stat, a read and a write,
//! and the differences between them are in authentication and in what evidence
//! they can give that a file is already there. Apache OpenDAL already models
//! exactly that, so this crate is thin by design: a validated
//! [`RemoteTarget`], a secret bundle that refuses to print itself, and the
//! builder wiring that joins them.
//!
//! It was chosen over `s3sync`, which the work started out aimed at. `s3sync`
//! speaks only S3 — WebDAV and SCP would have needed a second library and a
//! second credential model regardless — and it locks 359 crates where OpenDAL
//! locks 222 for all three protocols. The full reasoning, including the
//! measurements, is in `nato-5gb`.
//!
//! ## What is deliberately not here
//!
//! - **No credential storage.** This crate is *given* secrets; the Keychain is
//!   `src-tauri`'s. A library that reaches into the Keychain cannot be tested
//!   without one.
//! - **No scheduling, no queue, no progress events.** Those need an app
//!   lifetime and an event bus, and `crates/` may not depend on Tauri.
//! - **No deletion of remote data.** Nothing in this app removes an object it
//!   did not just write. The local sync is additive by policy
//!   (`src-tauri/src/sync.rs`); making the remote one destructive because the
//!   protocol happens to support `DELETE` would be a much larger promise than
//!   the one the user agreed to.
//!
//! ## The three things worth knowing before changing this
//!
//! 1. **A multipart S3 ETag is not an MD5.** Comparing one to a local digest
//!    reports "differs" for every large file that is in fact identical. See
//!    [`backend::usable_etag`].
//! 2. **There is no free-space check on a bucket.** The local planner refuses
//!    a copy that will not fit; a remote one cannot know, and must say so
//!    rather than report zero. See [`plan::RemotePlan::destination_available`].
//! 3. **A remote key is UTF-8; a macOS filename is bytes.** Not every local
//!    file can be named remotely, and the ones that cannot are skipped
//!    explicitly. See [`plan`].

#![forbid(unsafe_code)]

pub mod backend;
pub mod credentials;
pub mod plan;
pub mod profile;
pub mod target;

pub use backend::{Comparison, Remote, RemoteEntry, RemoteError, connect};
pub use credentials::{RemoteCredentials, Secret};
pub use plan::{RemotePlan, RemoteReason, RemoteSyncEntry, RemoteWarning};
pub use profile::{PROFILES, Profile};
pub use target::{RemoteKind, RemoteTarget, TargetError};
