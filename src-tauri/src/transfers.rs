//! A queue of uploads that outlives the window it was started from.
//!
//! The local sync in [`crate::sync`] is one blocking call that returns when it
//! is done, and for a local copy that is right: it runs at disk speed and the
//! user waits. A remote copy does not. Uploading 400 GB to a home NAS is an
//! overnight job, and a design where closing the window loses it — or where the
//! progress bar is the only record that it happened — is not a design for the
//! thing people actually do with this.
//!
//! So: jobs are records, not calls. They are written to disk when they change,
//! they come back when the app restarts, and the UI reads a list rather than
//! awaiting a future.
//!
//! ## Resuming is re-planning, and that is not a shortcut
//!
//! There is no partial-transfer bookkeeping here — no byte offsets, no resume
//! tokens, no manifest of what landed. A job that resumes simply plans again,
//! and planning already answers *what does the destination not have*. Files
//! that made it are found on the far side and skipped; the one file that was
//! mid-flight when the power went out is either absent (re-uploaded) or present
//! at the wrong length (re-uploaded, because size differs).
//!
//! This is why the additive, never-delete semantics of the planner are load
//! bearing rather than merely cautious. A mirroring sync could not do this: it
//! would have to distinguish "not yet uploaded" from "deleted at the source
//! since", and getting that wrong deletes data. An additive sync cannot get it
//! wrong, so restart-safety comes free and the crash-recovery path is the same
//! code as the ordinary path — which means it is tested every time anything is.
//!
//! ## What a restart does to an unfinished job
//!
//! Every job that was queued, planning or running comes back **paused**.
//! Nothing here starts on launch.
//!
//! For a running job the reason is obvious: the process died mid-upload, what
//! reached the far side is unknown until something looks, and resuming
//! unasked would start network traffic on a connection that may now be
//! metered.
//!
//! A merely *queued* job is demoted for a less obvious reason, and it is a
//! design consequence rather than caution. Since nothing auto-starts, a job
//! restored as `Queued` would be one no worker will ever pick up and that the
//! UI offers no button for — `Queued` is not a resumable state, because in
//! normal operation it lasts milliseconds before a worker claims it. Paused is
//! the state that has a Resume button, so paused is the honest place to put
//! anything the user still has to decide about.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rdirstat_remote::plan::{OnDiffer, RemoteCompare, RemoteSyncEntry};
use rdirstat_remote::{Remote, RemoteError};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// The file the queue is kept in, next to `settings.json`.
const FILE_NAME: &str = "transfers.json";

/// How many jobs are kept once they have finished.
///
/// A finished job is a receipt — what went where, when, and what failed. Worth
/// keeping, but not forever: past this the oldest are dropped.
const MAX_FINISHED: usize = 50;

/// How many parts of a *single* object may be in flight at once.
///
/// This is OpenDAL's `concurrent`, and it bounds the multipart upload of one
/// file — not the number of files. Four sits under the per-target connection
/// limit in `rdirstat-remote`, so a big file can use the link properly without
/// starving the rest of the queue.
///
/// Files themselves are uploaded one at a time. That is deliberate for a first
/// landing: per-file parallelism would make the progress counter and the
/// cancellation check race each other, and the measured win on a *network*
/// destination is far smaller than it was for the local per-file `ditto` loop
/// (nato-b6h), where 75% of the cost was fork/exec rather than bandwidth.
const PARTS_IN_FLIGHT: usize = 4;

/// Bytes per chunk handed to the writer.
///
/// 8 MiB because it is above S3's 5 MiB multipart minimum with room to spare,
/// and because a smaller chunk makes a large upload a long series of small
/// requests whose per-request overhead dominates. It is also the granularity at
/// which cancellation is noticed, which is the other reason not to make it
/// huge: a 64 MiB chunk on a slow link is a minute of a Cancel button doing
/// nothing visible.
const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// How often progress is written and emitted while a job runs.
///
/// A 100,000-file job would otherwise write the queue file 100,000 times.
const PROGRESS_EVERY: u64 = 32;

/// A job's identifier. Monotonic within a run, persisted across runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub(crate) struct TransferId(u64);

impl TransferId {
    #[must_use]
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// Where a job is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobState {
    /// Accepted, not started. Waiting for a worker slot.
    Queued,
    /// Walking the source and listing the destination. No bytes moving yet, and
    /// worth its own state because on a large tree it is where the time goes
    /// before anything appears to happen.
    Planning,
    Running,
    /// Stopped by the user, or by the process exiting. Resumable.
    Paused,
    /// Finished. `failures` may still be non-empty — a job that copied 9,998 of
    /// 10,000 files is done, and the two are named.
    Done,
    /// Could not proceed at all: the destination was unreachable, the source
    /// vanished, the credentials were refused.
    Failed,
    /// Stopped by the user and not resumable. What was already uploaded stays.
    Cancelled,
}

impl JobState {
    /// Whether this job is doing something right now.
    #[must_use]
    pub(crate) const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Planning | Self::Running)
    }

    /// Whether the user can still start it.
    #[must_use]
    pub(crate) const fn is_resumable(self) -> bool {
        matches!(self, Self::Paused | Self::Failed)
    }
}

/// One file that did not make it, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct TransferFailure {
    pub relative_path: String,
    pub reason: String,
}

/// A queued or completed upload.
///
/// Serialised verbatim into `transfers.json`, so every field here is one the
/// app is willing to still understand after a restart and an upgrade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct TransferJob {
    pub id: TransferId,
    /// The local folder being uploaded.
    pub source: String,
    /// The saved target's name — not its resolved address. A target the user
    /// re-pointed at a different bucket should send the *next* run of this job
    /// to the new one, and a job holding a stale endpoint would not.
    pub target_name: String,
    /// The target's address at the time the job was made, for display only.
    pub destination: String,
    pub compare: RemoteCompare,
    pub on_differ: OnDiffer,
    pub state: JobState,
    /// Files the most recent plan said to copy. Zero until the first plan.
    pub files_total: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub bytes_done: u64,
    /// Capped, because a job whose destination went away would otherwise
    /// record one failure per file and grow the queue file without bound.
    pub failures: Vec<TransferFailure>,
    pub failures_truncated: bool,
    /// Why the job as a whole stopped, when it was not per-file.
    pub message: Option<String>,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
}

/// Ceiling on recorded per-file failures.
const MAX_FAILURES: usize = 200;

impl TransferJob {
    fn record_failure(&mut self, relative_path: String, reason: String) {
        if self.failures.len() >= MAX_FAILURES {
            self.failures_truncated = true;
            return;
        }
        self.failures.push(TransferFailure { relative_path, reason });
    }
}

/// The queue as it sits on disk.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Queue {
    #[serde(default)]
    jobs: Vec<TransferJob>,
    #[serde(default)]
    next_id: u64,
}

/// Per-job control flags, held only while the process runs.
///
/// Deliberately not persisted: a paused job's pause is recorded in its
/// [`JobState`], and a *stop* flag that outlived the process would be a way for
/// a job to come back already-cancelled without anybody having cancelled it.
#[derive(Debug, Default)]
struct Control {
    /// Set to stop the job. `cancelled` distinguishes "pause" from "cancel"
    /// once the worker notices.
    stop: AtomicBool,
    cancelled: AtomicBool,
}

/// The live queue: the persisted jobs plus the running tasks' controls.
#[derive(Debug)]
pub(crate) struct TransferManager {
    app_data: PathBuf,
    /// One lock over the whole queue. Contention is not a concern — mutations
    /// are a user clicking a button and a worker ticking every
    /// [`PROGRESS_EVERY`] files — and one lock is why a job cannot be observed
    /// half-updated.
    inner: Mutex<Queue>,
    controls: Mutex<BTreeMap<TransferId, Arc<Control>>>,
    next_id: AtomicU64,
}

impl TransferManager {
    /// Loads the queue from disk, demoting anything unfinished to `Paused`.
    ///
    /// See the module docs. Two reasons, both structural: nothing here knows
    /// what the far side received from an interrupted upload, and nothing
    /// auto-starts on launch — so `Paused`, which has a Resume button, is the
    /// only restored state a user can act on.
    #[must_use]
    pub(crate) fn load(app_data: &Path) -> Self {
        let mut queue = read_queue(app_data);
        let mut demoted = 0_u32;
        for job in &mut queue.jobs {
            if job.state.is_active() {
                job.state = JobState::Paused;
                job.message = Some(
                    "This transfer was interrupted when the app closed. Resuming checks what \
                     is already at the destination and continues from there."
                        .to_owned(),
                );
                demoted += 1;
            }
        }
        if demoted > 0 {
            tracing::info!(demoted, "interrupted transfers were restored as paused");
            // Persisted immediately: if this run also dies, the next one must
            // see paused jobs rather than repeat the demotion message.
            if let Err(error) = write_queue(app_data, &queue) {
                tracing::warn!(%error, "the restored transfer queue could not be written");
            }
        }
        let next_id = queue
            .next_id
            .max(queue.jobs.iter().map(|job| job.id.0 + 1).max().unwrap_or(1));
        Self {
            app_data: app_data.to_path_buf(),
            inner: Mutex::new(queue),
            controls: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(next_id),
        }
    }

    /// Every job, newest first.
    pub(crate) async fn list(&self) -> Vec<TransferJob> {
        let queue = self.inner.lock().await;
        let mut jobs = queue.jobs.clone();
        // Negated so the newest sorts first; `sort_unstable_by_key` cannot
        // express a descending order without one.
        jobs.sort_unstable_by_key(|job| std::cmp::Reverse(job.created_unix_ms));
        jobs
    }

    /// Adds a job in the `Queued` state and returns it.
    pub(crate) async fn enqueue(
        &self,
        source: &Path,
        target_name: &str,
        destination: &str,
        compare: RemoteCompare,
        on_differ: OnDiffer,
        now_unix_ms: i64,
    ) -> TransferJob {
        let id = TransferId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let job = TransferJob {
            id,
            source: source.to_string_lossy().into_owned(),
            target_name: target_name.to_owned(),
            destination: destination.to_owned(),
            compare,
            on_differ,
            state: JobState::Queued,
            files_total: 0,
            bytes_total: 0,
            files_done: 0,
            bytes_done: 0,
            failures: Vec::new(),
            failures_truncated: false,
            message: None,
            created_unix_ms: now_unix_ms,
            updated_unix_ms: now_unix_ms,
        };
        let mut queue = self.inner.lock().await;
        queue.jobs.push(job.clone());
        queue.next_id = self.next_id.load(Ordering::Relaxed);
        prune(&mut queue);
        self.persist(&queue);
        job
    }

    /// Asks a running job to stop.
    ///
    /// Returns false when there was no such job. Setting the flag is all this
    /// does — the worker notices between chunks and writes the final state, so
    /// that the state on disk is always one a worker actually reached.
    pub(crate) async fn request_stop(&self, id: TransferId, cancel: bool) -> bool {
        let controls = self.controls.lock().await;
        let Some(control) = controls.get(&id) else {
            // Not running. Cancelling a queued or paused job is still
            // meaningful, and is handled by the caller through `set_state`.
            return false;
        };
        if cancel {
            control.cancelled.store(true, Ordering::SeqCst);
        }
        control.stop.store(true, Ordering::SeqCst);
        true
    }

    /// Moves a job to a state directly, for the transitions no worker is
    /// involved in — cancelling something that never started, or resuming.
    pub(crate) async fn set_state(&self, id: TransferId, state: JobState, now_unix_ms: i64) -> Option<TransferJob> {
        let mut queue = self.inner.lock().await;
        let job = queue.jobs.iter_mut().find(|job| job.id == id)?;
        job.state = state;
        job.updated_unix_ms = now_unix_ms;
        if state == JobState::Queued {
            // A fresh attempt: clear the previous run's verdict so the UI does
            // not show a stale failure beside a running bar. The counters go
            // too, because the next plan recomputes them from the far side.
            job.message = None;
            job.failures.clear();
            job.failures_truncated = false;
            job.files_done = 0;
            job.bytes_done = 0;
        }
        let job = job.clone();
        self.persist(&queue);
        Some(job)
    }

    /// Removes finished jobs. Running ones are left alone.
    pub(crate) async fn clear_finished(&self, now_unix_ms: i64) -> usize {
        let _ = now_unix_ms;
        let mut queue = self.inner.lock().await;
        let before = queue.jobs.len();
        queue
            .jobs
            .retain(|job| job.state.is_active() || job.state == JobState::Paused);
        let removed = before - queue.jobs.len();
        if removed > 0 {
            self.persist(&queue);
        }
        removed
    }

    /// One job by id.
    pub(crate) async fn get(&self, id: TransferId) -> Option<TransferJob> {
        self.inner.lock().await.jobs.iter().find(|job| job.id == id).cloned()
    }

    /// Registers a control handle for a job about to run, and hands it back.
    async fn begin(&self, id: TransferId) -> Arc<Control> {
        let control = Arc::new(Control::default());
        self.controls.lock().await.insert(id, Arc::clone(&control));
        control
    }

    async fn end(&self, id: TransferId) {
        self.controls.lock().await.remove(&id);
    }

    /// Applies a change to one job and writes the queue.
    async fn update(&self, id: TransferId, edit: impl FnOnce(&mut TransferJob)) -> Option<TransferJob> {
        let mut queue = self.inner.lock().await;
        let job = queue.jobs.iter_mut().find(|job| job.id == id)?;
        edit(job);
        let job = job.clone();
        self.persist(&queue);
        Some(job)
    }

    /// Writes the queue, logging rather than propagating.
    ///
    /// A queue that cannot be written is a real problem, but it is not one the
    /// user can do anything about mid-transfer, and aborting a running upload
    /// because a bookkeeping file failed would trade a recoverable annoyance
    /// for a lost hour of bandwidth.
    fn persist(&self, queue: &Queue) {
        if let Err(error) = write_queue(&self.app_data, queue) {
            tracing::warn!(%error, "the transfer queue could not be written");
        }
    }
}

/// Runs one job to completion, or until it is asked to stop.
///
/// Split out from [`TransferManager`] because it is the only part that touches
/// the network, and keeping it a free function means the manager's own
/// behaviour — enqueue, pause, restart-demotion, pruning — is testable without
/// one.
pub(crate) async fn run_job(
    manager: &TransferManager,
    remote: &Remote,
    id: TransferId,
    now_unix_ms: impl Fn() -> i64 + Send + Sync,
) -> Result<(), RemoteError> {
    let control = manager.begin(id).await;
    let outcome = drive(manager, remote, id, &control, &now_unix_ms).await;
    manager.end(id).await;
    outcome
}

/// Lists the destination once and works out what it is missing.
///
/// The whole planning half of a job, split out because it is the part with no
/// bytes moving and it is where the wall clock goes on a large tree: one
/// recursive listing, then a purely local walk against it. See
/// [`rdirstat_remote::plan`] for why it is that way round rather than one
/// `stat` per file.
///
/// # Errors
///
/// The message to record against the job. Both failure modes — an unreachable
/// destination and a walk that panicked — are things the user must be told
/// about rather than retried through, so they collapse to one string here.
async fn decide_what_to_send(
    remote: &Remote,
    source: &Path,
    job: &TransferJob,
) -> Result<rdirstat_remote::RemotePlan, String> {
    let (listing, listing_truncated) = remote.list().await.map_err(|error| error.to_string())?;

    let comparison = remote.comparison();
    let source = source.to_path_buf();
    let destination = remote.display().to_owned();
    let (compare, on_differ) = (job.compare, job.on_differ);

    // The walk is blocking filesystem work, and under Verify it reads every
    // same-sized file in full to hash it, so it does not belong on the async
    // executor alongside the network tasks.
    tokio::task::spawn_blocking(move || {
        rdirstat_remote::plan::plan(rdirstat_remote::plan::RemotePlanRequest {
            source: &source,
            destination: &destination,
            listing: &listing,
            listing_truncated,
            available_comparison: comparison,
            compare,
            on_differ,
            // UNCAPPED. The review pass uses the display cap; this one is the
            // actual copy, and a capped list here would upload 5,000 of 40,000
            // files and then report the job Done.
            max_entries: usize::MAX,
        })
    })
    .await
    .map_err(|error| error.to_string())
}

async fn drive(
    manager: &TransferManager,
    remote: &Remote,
    id: TransferId,
    control: &Control,
    now_unix_ms: &(impl Fn() -> i64 + Send + Sync),
) -> Result<(), RemoteError> {
    let Some(job) = manager.get(id).await else {
        return Ok(());
    };
    let source = PathBuf::from(&job.source);

    manager
        .update(id, |job| {
            job.state = JobState::Planning;
            job.updated_unix_ms = now_unix_ms();
        })
        .await;

    let planned = match decide_what_to_send(remote, &source, &job).await {
        Ok(planned) => planned,
        Err(reason) => {
            manager
                .update(id, |job| {
                    job.state = JobState::Failed;
                    job.message = Some(reason);
                    job.updated_unix_ms = now_unix_ms();
                })
                .await;
            return Ok(());
        }
    };

    manager
        .update(id, |job| {
            job.state = JobState::Running;
            job.files_total = planned.total_to_copy;
            job.bytes_total = planned.bytes_to_copy;
            job.files_done = 0;
            job.bytes_done = 0;
            job.updated_unix_ms = now_unix_ms();
        })
        .await;

    let entries = planned.entries.clone();

    let mut done = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries {
        if control.stop.load(Ordering::SeqCst) {
            let cancelled = control.cancelled.load(Ordering::SeqCst);
            manager
                .update(id, |job| {
                    job.state = if cancelled {
                        JobState::Cancelled
                    } else {
                        JobState::Paused
                    };
                    job.updated_unix_ms = now_unix_ms();
                })
                .await;
            return Ok(());
        }

        let key = entry.relative_path.clone();
        match upload_one(remote, &source, &entry, control).await {
            Ok(true) => {
                done += 1;
                bytes += entry.bytes;
            }
            // Stopped mid-file. The partial object was aborted, so the
            // destination holds either the old copy or nothing — never a
            // truncated file presented as complete.
            Ok(false) => {
                let cancelled = control.cancelled.load(Ordering::SeqCst);
                manager
                    .update(id, |job| {
                        job.state = if cancelled {
                            JobState::Cancelled
                        } else {
                            JobState::Paused
                        };
                        job.files_done = done;
                        job.bytes_done = bytes;
                        job.updated_unix_ms = now_unix_ms();
                    })
                    .await;
                return Ok(());
            }
            Err(error) => {
                let reason = error.to_string();
                manager.update(id, |job| job.record_failure(key, reason)).await;
            }
        }

        if done.is_multiple_of(PROGRESS_EVERY) {
            manager
                .update(id, |job| {
                    job.files_done = done;
                    job.bytes_done = bytes;
                    job.updated_unix_ms = now_unix_ms();
                })
                .await;
        }
    }

    manager
        .update(id, |job| {
            job.state = JobState::Done;
            job.files_done = done;
            job.bytes_done = bytes;
            job.updated_unix_ms = now_unix_ms();
        })
        .await;
    Ok(())
}

/// Streams one file to the destination.
///
/// Returns `Ok(false)` when it stopped early because the job was asked to.
///
/// # Errors
///
/// [`RemoteError`] for anything that went wrong with this one file. The caller
/// records it and moves on: one unreadable file must not end a 400 GB job.
async fn upload_one(
    remote: &Remote,
    source: &Path,
    entry: &RemoteSyncEntry,
    control: &Control,
) -> Result<bool, RemoteError> {
    use tokio::io::AsyncReadExt as _;

    let local = source.join(&entry.relative_path);
    let mut file = tokio::fs::File::open(&local)
        .await
        .map_err(|error| RemoteError::Operation {
            operation: "read",
            path: entry.relative_path.clone(),
            reason: error.to_string(),
        })?;

    let mut writer = remote
        .operator()
        .writer_with(&entry.relative_path)
        .chunk(CHUNK_BYTES)
        .concurrent(PARTS_IN_FLIGHT)
        .await
        .map_err(|error| RemoteError::Operation {
            operation: "write",
            path: entry.relative_path.clone(),
            reason: error.to_string(),
        })?;

    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        // Checked per chunk, which is what makes Cancel take effect inside a
        // single large file rather than only between files.
        if control.stop.load(Ordering::SeqCst) {
            // `abort`, not `close`: closing would commit whatever parts made it
            // and leave a short object that the next plan compares by size and
            // may well accept. Aborting leaves the key absent.
            drop(writer.abort().await);
            return Ok(false);
        }

        let read = file.read(&mut buffer).await.map_err(|error| RemoteError::Operation {
            operation: "read",
            path: entry.relative_path.clone(),
            reason: error.to_string(),
        })?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).unwrap_or_default().to_vec();
        if let Err(error) = writer.write(chunk).await {
            drop(writer.abort().await);
            return Err(RemoteError::Operation {
                operation: "write",
                path: entry.relative_path.clone(),
                reason: error.to_string(),
            });
        }
    }

    writer
        .close()
        .await
        .map(|_| true)
        .map_err(|error| RemoteError::Operation {
            operation: "write",
            path: entry.relative_path.clone(),
            reason: error.to_string(),
        })
}

/// Drops the oldest finished jobs past [`MAX_FINISHED`].
fn prune(queue: &mut Queue) {
    let finished: Vec<usize> = queue
        .jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| matches!(job.state, JobState::Done | JobState::Failed | JobState::Cancelled))
        .map(|(index, _)| index)
        .collect();
    if finished.len() <= MAX_FINISHED {
        return;
    }
    // Oldest first, so the ones removed are the ones furthest from useful.
    let mut oldest = finished;
    oldest.sort_by_key(|index| queue.jobs.get(*index).map_or(i64::MAX, |job| job.created_unix_ms));
    let excess = oldest.len() - MAX_FINISHED;
    let mut doomed: Vec<usize> = oldest.into_iter().take(excess).collect();
    doomed.sort_unstable_by(|left, right| right.cmp(left));
    for index in doomed {
        queue.jobs.remove(index);
    }
}

fn read_queue(app_data: &Path) -> Queue {
    let path = app_data.join(FILE_NAME);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Queue::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "the transfer queue is unreadable; starting empty");
        Queue::default()
    })
}

fn write_queue(app_data: &Path, queue: &Queue) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(app_data)?;
    let text = serde_json::to_string_pretty(queue).map_err(std::io::Error::other)?;
    let staging = app_data.join(format!(".{FILE_NAME}.{}", std::process::id()));
    std::fs::write(&staging, text.as_bytes())?;
    std::fs::rename(&staging, app_data.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(dir: &Path) -> TransferManager {
        TransferManager::load(dir)
    }

    async fn enqueue(manager: &TransferManager, name: &str, at: i64) -> TransferJob {
        manager
            .enqueue(
                Path::new("/tmp/source"),
                name,
                "s3://bucket/prefix/",
                RemoteCompare::Quick,
                OnDiffer::Skip,
                at,
            )
            .await
    }

    // The queue is the point: a job outlives the window it was made in.
    #[tokio::test]
    async fn a_job_survives_a_restart_with_its_details_intact() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let first = manager(dir.path());
        let job = enqueue(&first, "backup", 1_000).await;
        drop(first);

        let second = manager(dir.path());
        let jobs = second.list().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        assert_eq!(jobs[0].target_name, "backup");
        assert_eq!(jobs[0].source, "/tmp/source");
    }

    // Nothing starts by itself on launch, so a restored job has to be in the
    // one unfinished state the UI gives a button to. `Queued` is not that
    // state, and a job restored into it would sit there forever.
    #[tokio::test]
    async fn every_restored_job_is_one_the_user_can_actually_restart() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let first = manager(dir.path());
        let queued = enqueue(&first, "queued", 1_000).await;
        let planning = enqueue(&first, "planning", 1_001).await;
        let running = enqueue(&first, "running", 1_002).await;
        first.set_state(planning.id, JobState::Planning, 1_500).await;
        first.set_state(running.id, JobState::Running, 1_500).await;
        drop(first);

        let jobs = manager(dir.path()).list().await;
        assert_eq!(jobs.len(), 3);
        for job in &jobs {
            assert_eq!(
                job.state,
                JobState::Paused,
                "{} was restored unrestartable",
                job.target_name
            );
            assert!(job.state.is_resumable());
        }
        let _ = queued;
    }

    // The restart rule the module docs argue for: a job that was moving bytes
    // when the process died must not silently start moving them again.
    #[tokio::test]
    async fn a_running_job_comes_back_paused_and_says_why() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let first = manager(dir.path());
        let job = enqueue(&first, "backup", 1_000).await;
        first.set_state(job.id, JobState::Running, 2_000).await;
        drop(first);

        let second = manager(dir.path());
        let jobs = second.list().await;
        assert_eq!(jobs[0].state, JobState::Paused);
        let message = jobs[0].message.as_deref().expect("an explanation");
        assert!(message.contains("interrupted"), "{message}");
    }

    #[tokio::test]
    async fn planning_is_also_demoted_on_restart() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let first = manager(dir.path());
        let job = enqueue(&first, "backup", 1_000).await;
        first.set_state(job.id, JobState::Planning, 2_000).await;
        drop(first);

        assert_eq!(manager(dir.path()).list().await[0].state, JobState::Paused);
    }

    #[tokio::test]
    async fn a_finished_job_is_not_demoted() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let first = manager(dir.path());
        let job = enqueue(&first, "backup", 1_000).await;
        first.set_state(job.id, JobState::Done, 2_000).await;
        drop(first);

        let jobs = manager(dir.path()).list().await;
        assert_eq!(jobs[0].state, JobState::Done);
        assert_eq!(jobs[0].message, None);
    }

    // Ids must not be reused after a restart, or a stale UI handle would
    // control somebody else's job.
    #[tokio::test]
    async fn ids_keep_going_up_across_a_restart() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let first = manager(dir.path());
        let one = enqueue(&first, "a", 1_000).await;
        let two = enqueue(&first, "b", 1_001).await;
        assert!(two.id > one.id);
        drop(first);

        let second = manager(dir.path());
        let three = enqueue(&second, "c", 1_002).await;
        assert!(three.id > two.id, "{three:?} should outrank {two:?}");
    }

    #[tokio::test]
    async fn resuming_clears_the_previous_attempts_verdict() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let manager = manager(dir.path());
        let job = enqueue(&manager, "backup", 1_000).await;
        manager
            .update(job.id, |job| {
                job.state = JobState::Failed;
                job.message = Some("the bucket was unreachable".to_owned());
                job.record_failure("a.txt".to_owned(), "timed out".to_owned());
                job.files_done = 7;
            })
            .await;

        let resumed = manager
            .set_state(job.id, JobState::Queued, 2_000)
            .await
            .expect("the job exists");
        assert_eq!(resumed.state, JobState::Queued);
        assert_eq!(resumed.message, None);
        assert!(resumed.failures.is_empty());
        assert_eq!(resumed.files_done, 0);
    }

    #[tokio::test]
    async fn clearing_keeps_what_is_still_going() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let manager = manager(dir.path());
        let running = enqueue(&manager, "running", 1_000).await;
        let paused = enqueue(&manager, "paused", 1_001).await;
        let done = enqueue(&manager, "done", 1_002).await;
        manager.set_state(running.id, JobState::Running, 2_000).await;
        manager.set_state(paused.id, JobState::Paused, 2_000).await;
        manager.set_state(done.id, JobState::Done, 2_000).await;

        assert_eq!(manager.clear_finished(3_000).await, 1);
        let left: Vec<JobState> = manager.list().await.iter().map(|job| job.state).collect();
        assert!(!left.contains(&JobState::Done));
        assert!(left.contains(&JobState::Running));
        assert!(left.contains(&JobState::Paused), "a paused job is not finished");
    }

    #[tokio::test]
    async fn a_failure_list_stops_growing_but_says_that_it_did() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let manager = manager(dir.path());
        let job = enqueue(&manager, "backup", 1_000).await;
        manager
            .update(job.id, |job| {
                for index in 0..(MAX_FAILURES + 10) {
                    job.record_failure(format!("f{index}"), "nope".to_owned());
                }
            })
            .await;

        let job = manager.get(job.id).await.expect("the job exists");
        assert_eq!(job.failures.len(), MAX_FAILURES);
        assert!(job.failures_truncated);
    }

    #[tokio::test]
    async fn stopping_a_job_that_is_not_running_reports_that_it_did_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let manager = manager(dir.path());
        let job = enqueue(&manager, "backup", 1_000).await;
        assert!(!manager.request_stop(job.id, true).await);
    }

    #[tokio::test]
    async fn stopping_a_running_job_sets_the_flag_the_worker_watches() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let manager = manager(dir.path());
        let job = enqueue(&manager, "backup", 1_000).await;
        let control = manager.begin(job.id).await;

        assert!(manager.request_stop(job.id, false).await);
        assert!(control.stop.load(Ordering::SeqCst));
        assert!(!control.cancelled.load(Ordering::SeqCst), "pause is not cancel");

        assert!(manager.request_stop(job.id, true).await);
        assert!(control.cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn finished_jobs_are_pruned_oldest_first() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let manager = manager(dir.path());
        for index in 0..(MAX_FINISHED + 5) {
            let at = 1_000 + i64::try_from(index).expect("a small loop counter fits an i64");
            let job = enqueue(&manager, &format!("j{index}"), at).await;
            manager.set_state(job.id, JobState::Done, 2_000).await;
        }
        // One more enqueue triggers the prune.
        enqueue(&manager, "trigger", 9_999).await;

        let jobs = manager.list().await;
        let finished = jobs.iter().filter(|job| job.state == JobState::Done).count();
        assert!(finished <= MAX_FINISHED, "{finished} finished jobs survived");
        assert!(
            jobs.iter()
                .any(|job| job.target_name == format!("j{}", MAX_FINISHED + 4)),
            "the newest finished job must survive"
        );
    }

    #[tokio::test]
    async fn a_damaged_queue_file_costs_the_queue_and_nothing_else() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join(FILE_NAME), b"{ not json").expect("the fixture should write");
        assert!(manager(dir.path()).list().await.is_empty());
    }

    #[test]
    fn an_active_job_is_not_resumable_and_the_reverse() {
        for state in [JobState::Queued, JobState::Planning, JobState::Running] {
            assert!(state.is_active(), "{state:?}");
            assert!(!state.is_resumable(), "{state:?}");
        }
        for state in [JobState::Paused, JobState::Failed] {
            assert!(!state.is_active(), "{state:?}");
            assert!(state.is_resumable(), "{state:?}");
        }
        for state in [JobState::Done, JobState::Cancelled] {
            assert!(!state.is_active(), "{state:?}");
            assert!(!state.is_resumable(), "{state:?}");
        }
    }
}
