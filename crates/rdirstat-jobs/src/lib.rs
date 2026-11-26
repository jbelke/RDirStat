//! Wire contract of the drive-resident backup service (`rdirstat backup
//! serve`): the files it keeps on the backup drive and the HTTP API it
//! answers. Types only: no I/O, no async, one dependency (`serde`), so the
//! desktop app can be a client without linking the engine.
//!
//! # On the drive
//!
//! ```text
//! <dest>/.rdirstat/
//!   run/
//!     lock                    LockInfo (JSON); one service per drive
//!     serve.log               stdout/stderr of a `--detach`ed service
//!   jobs/<id>/
//!     job.json                JobSpec, written once when the job is created
//!     state.json              JobState, replaced atomically (tmp + rename)
//!     events.jsonl            Event, one JSON object per line, append-only
//!     control                 Control, one word: run | pause | cancel
//!     plans/<label>-r<N>.json the plan file of each round
//! ```
//!
//! - **`job.json`** ([`JobSpec`]) is what to back up: source labels and
//!   roots, the destination, the rules file, the floor, how many rounds a
//!   label gets. It is never rewritten after creation.
//! - **`events.jsonl`** ([`Event`]) is the durable record. Every line has a
//!   `seq` that starts at 1 and increases by exactly one per line, so a reader
//!   that has seen `seq = n` resumes with `?after=n` (or by skipping `n`
//!   lines) without gaps or repeats. One line per item outcome, per phase
//!   change, per finished round, per error. Each line is `fsync`ed before the
//!   next item starts. A line may be missing its trailing newline only after
//!   a crash mid-write; readers skip a last line that does not parse.
//! - **`state.json`** ([`JobState`]) is a snapshot for cheap reads: phase,
//!   current label and round, counts, bytes, last error, and the `seq` of the
//!   last event it reflects. It is written at every phase change and at most
//!   about once a second during a round, so it may trail `events.jsonl` by a
//!   few items; `events.jsonl` wins when they disagree.
//! - **`control`** ([`Control`]) is how a pause or cancel survives a service
//!   restart. The service writes it when the API is asked; the job runner
//!   reads it between items.
//!
//! # The API socket is not on the drive
//!
//! A backup drive is usually mounted with ownership ignored (`noowners`),
//! so a mode on a file there is not enforced against other local users. The
//! socket therefore lives on the boot disk, in a per-user directory the
//! service creates with mode 0700 and refuses to use unless the current user
//! owns it, keyed by the destination's volume UUID ([`socket_key`]):
//!
//! ```text
//! ~/Library/Application Support/rdirstat/run/   mode 0700
//!   <volume-uuid>.sock                          mode 0600
//! ```
//!
//! A Unix socket path must fit in 104 bytes (`sun_path`, less its NUL). When
//! the home directory's path is too long for that, the socket goes to
//! `$TMPDIR/rds/` instead (a short name, because `$TMPDIR` itself is about 50
//! bytes; on macOS `$TMPDIR` is already per-user
//! and 0700). [`local_socket_path`] makes that choice; the service and every
//! client make it from the same `$HOME`, `$TMPDIR` and volume UUID, so they
//! agree without the drive telling them where to connect (a path read from
//! the drive could be planted by anyone who can write to it).
//!
//! While the drive is disconnected the files cannot be written. The service
//! keeps assigning `seq` in memory and appends the held lines, in order, when
//! the drive is back. It never creates `<dest>/.rdirstat` itself while the
//! drive is away, so nothing lands on the boot disk under `/Volumes`.
//!
//! # API (`/v1`)
//!
//! | Method and path | Body | Answer |
//! | --- | --- | --- |
//! | `GET /v1/health` | | [`Health`] |
//! | `GET /v1/jobs` | | [`JobList`] |
//! | `POST /v1/jobs` | [`NewJob`] | `201` [`JobSummary`] |
//! | `GET /v1/jobs/{id}` | | [`JobSummary`] |
//! | `POST /v1/jobs/{id}/pause` | | [`JobSummary`] |
//! | `POST /v1/jobs/{id}/resume` | | [`JobSummary`] |
//! | `POST /v1/jobs/{id}/cancel` | | [`JobSummary`] |
//! | `GET /v1/jobs/{id}/events?after=<seq>&wait=<s>&limit=<n>` | | [`EventsPage`] |
//! | `POST /v1/shutdown` | | `202` [`Health`] |
//!
//! Errors are a status code and an [`ApiError`]. `events` is a long-poll:
//! with nothing after `after`, it waits up to `wait` seconds (default
//! [`EVENTS_DEFAULT_WAIT_SECS`], at most [`EVENTS_MAX_WAIT_SECS`]) for the next
//! event and answers with an empty page if none comes. `limit` defaults to and
//! is capped at [`EVENTS_MAX_LIMIT`].
//!
//! `resume` re-queues a job that is cancelled, incomplete, failed or stopped;
//! labels that did not finish get a fresh set of rounds. `cancel` and `pause`
//! take effect between items: the item being copied always finishes.
//! `shutdown` (and `SIGTERM`) finishes the current item, records the job as
//! [`Phase::Stopped`], and exits; the next service on this drive resumes it.
//!
//! # Compatibility
//!
//! Fields are only added, never renamed or retyped, within one
//! [`SCHEMA_VERSION`]. Readers must ignore unknown fields and unknown event
//! kinds.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Version of every file layout and API type in this crate.
pub const SCHEMA_VERSION: u32 = 1;
/// The API's path prefix.
pub const API_PREFIX: &str = "/v1";

/// `<dest>/.rdirstat`.
pub const RDIRSTAT_DIR: &str = ".rdirstat";
/// `<dest>/.rdirstat/jobs`.
pub const JOBS_DIR: &str = "jobs";
/// `<dest>/.rdirstat/run`.
pub const RUN_DIR: &str = "run";
/// The service's lock file in [`RUN_DIR`].
pub const LOCK_FILE: &str = "lock";
/// The per-user directory under `$HOME` that holds API sockets.
pub const LOCAL_RUN_DIR: &str = "Library/Application Support/rdirstat/run";
/// The per-user directory under `$TMPDIR` used when [`LOCAL_RUN_DIR`] makes a
/// socket path too long.
pub const TMP_RUN_DIR: &str = "rds";
/// Longest path `bind(2)` accepts for a Unix socket on macOS, less the NUL.
pub const SUN_PATH_MAX: usize = 103;
/// A detached service's log in [`RUN_DIR`].
pub const SERVE_LOG: &str = "serve.log";
/// In a job directory.
pub const JOB_FILE: &str = "job.json";
/// In a job directory.
pub const STATE_FILE: &str = "state.json";
/// In a job directory.
pub const EVENTS_FILE: &str = "events.jsonl";
/// In a job directory.
pub const CONTROL_FILE: &str = "control";
/// In a job directory: each round's plan file.
pub const PLANS_DIR: &str = "plans";

/// Rounds a label gets when [`NewJob::max_rounds`] is not given.
pub const DEFAULT_MAX_ROUNDS: u32 = 5;
/// How often a job looks for a missing source or destination.
pub const DEFAULT_MOUNT_POLL_MS: u64 = 10_000;
/// Long-poll wait when `wait` is not given.
pub const EVENTS_DEFAULT_WAIT_SECS: u64 = 25;
/// Longest long-poll wait.
pub const EVENTS_MAX_WAIT_SECS: u64 = 60;
/// Most events in one [`EventsPage`].
pub const EVENTS_MAX_LIMIT: usize = 1_000;

/// `<dest>/.rdirstat/run`.
#[must_use]
pub fn run_dir(dest: &Path) -> PathBuf {
    dest.join(RDIRSTAT_DIR).join(RUN_DIR)
}

/// The file stem of a destination's socket: its volume UUID when known and
/// made of hex digits and dashes (as `diskutil` reports one), otherwise
/// `path-` and a 64-bit FNV-1a hash of the destination path, so a volume
/// without a UUID still gets a stable, file-name-safe key.
#[must_use]
pub fn socket_key(volume_uuid: Option<&str>, dest: &Path) -> String {
    if let Some(uuid) = volume_uuid
        && !uuid.is_empty()
        && uuid.len() <= 64
        && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return uuid.to_ascii_uppercase();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dest.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("path-{hash:016x}")
}

/// Where the API socket for `key` may go, in order of preference:
/// `$HOME/`[`LOCAL_RUN_DIR`]`/<key>.sock`, then
/// `$TMPDIR/`[`TMP_RUN_DIR`]`/<key>.sock`. Relative or absent bases are
/// skipped.
#[must_use]
pub fn socket_candidates(home: Option<&Path>, tmpdir: Option<&Path>, key: &str) -> Vec<PathBuf> {
    let file = format!("{key}.sock");
    [
        home.map(|home| home.join(LOCAL_RUN_DIR)),
        tmpdir.map(|tmp| tmp.join(TMP_RUN_DIR)),
    ]
    .into_iter()
    .flatten()
    .filter(|dir| dir.is_absolute())
    .map(|dir| dir.join(&file))
    .collect()
}

/// The first of [`socket_candidates`] that fits in [`SUN_PATH_MAX`] bytes,
/// or `None` when none does.
#[must_use]
pub fn local_socket_path(home: Option<&Path>, tmpdir: Option<&Path>, key: &str) -> Option<PathBuf> {
    socket_candidates(home, tmpdir, key)
        .into_iter()
        .find(|path| path.as_os_str().len() <= SUN_PATH_MAX)
}

/// `<dest>/.rdirstat/jobs`.
#[must_use]
pub fn jobs_dir(dest: &Path) -> PathBuf {
    dest.join(RDIRSTAT_DIR).join(JOBS_DIR)
}

/// `<dest>/.rdirstat/jobs/<id>`.
#[must_use]
pub fn job_dir(dest: &Path, id: &str) -> PathBuf {
    jobs_dir(dest).join(id)
}

/// A job id is a path component: 1 to 64 of `[A-Za-z0-9._-]`, not starting
/// with `.`.
#[must_use]
pub fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// One source of a job: a volume (or directory) root and the label it is
/// mirrored under, `<dest>/Backups/<label>/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSource {
    /// Name under `Backups/`, e.g. `mac-data`.
    pub label: String,
    /// The source root plan paths are relative to.
    pub root: PathBuf,
}

/// `job.json`: what a job backs up. Written once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// [`SCHEMA_VERSION`] at the time of writing.
    pub schema_version: u32,
    /// The job directory's name.
    pub id: String,
    /// Milliseconds since the Unix epoch.
    pub created_at_ms: i64,
    /// Destination root: the backup volume's mount point.
    pub dest: PathBuf,
    /// Labels run in this order.
    pub sources: Vec<JobSource>,
    /// Skip/include rules file (TOML). Relative paths are relative to the job
    /// directory. `None`: the built-in Backup rules.
    #[serde(default)]
    pub rules: Option<PathBuf>,
    /// Smallest allocated size of a candidate, in bytes.
    pub floor: u64,
    /// Plan + apply rounds per label before it is called incomplete.
    pub max_rounds: u32,
    /// Cover what is not planned on its own with directory roll-ups.
    pub rollup: bool,
    /// Primary catalog directory. `None`: the desktop app's data dir.
    #[serde(default)]
    pub catalog: Option<PathBuf>,
    /// How often to look for a missing source or destination, in ms.
    pub mount_poll_ms: u64,
    /// Give up waiting for a missing volume after this many ms. `None`: wait
    /// until it comes back or the job is cancelled.
    #[serde(default)]
    pub mount_timeout_ms: Option<u64>,
    /// The profile this job was made from, if any.
    #[serde(default)]
    pub profile: Option<String>,
}

/// `POST /v1/jobs` body. The service fills in the id (when absent), the
/// destination (its own), and the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewJob {
    /// Must be a [`valid_job_id`] not already used. Default:
    /// `job-<created_at_ms>`.
    #[serde(default)]
    pub id: Option<String>,
    /// Must equal the service's destination when given.
    #[serde(default)]
    pub dest: Option<PathBuf>,
    /// At least one.
    pub sources: Vec<JobSource>,
    /// See [`JobSpec::rules`].
    #[serde(default)]
    pub rules: Option<PathBuf>,
    /// Bytes. Default: 100 MiB.
    #[serde(default)]
    pub floor: Option<u64>,
    /// Default [`DEFAULT_MAX_ROUNDS`].
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// Default `true`.
    #[serde(default)]
    pub rollup: Option<bool>,
    /// See [`JobSpec::catalog`].
    #[serde(default)]
    pub catalog: Option<PathBuf>,
    /// Default [`DEFAULT_MOUNT_POLL_MS`].
    #[serde(default)]
    pub mount_poll_ms: Option<u64>,
    /// See [`JobSpec::mount_timeout_ms`].
    #[serde(default)]
    pub mount_timeout_ms: Option<u64>,
    /// See [`JobSpec::profile`].
    #[serde(default)]
    pub profile: Option<String>,
}

/// Floor used when [`NewJob::floor`] is not given: 100 MiB.
pub const DEFAULT_FLOOR: u64 = 100 << 20;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Created or re-queued; not started.
    Queued,
    /// A source root or the destination is missing; polling for it.
    WaitingForMount,
    /// Walking a source and writing a round's plan.
    Planning,
    /// Copying a round's items.
    Applying,
    /// Held between items by a `pause`.
    Paused,
    /// Rebuilding `<dest>/index.parquet`.
    Indexing,
    /// Every label landed everything its last plan held.
    Done,
    /// Rounds ran out, or a round landed nothing new, with items not landed.
    Incomplete,
    /// Stopped by `cancel`.
    Cancelled,
    /// Stopped by `shutdown` or `SIGTERM`; resumes when a service starts.
    Stopped,
    /// A label was refused (bad pair, wrong volume, unreadable rules).
    Failed,
}

impl Phase {
    /// The job will not run again unless it is resumed.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Incomplete | Self::Cancelled | Self::Failed)
    }

    /// The job has work left that a running service picks up by itself:
    /// anything not terminal. A job in one of these phases with no live
    /// service was interrupted.
    #[must_use]
    pub const fn is_unfinished(self) -> bool {
        !self.is_terminal()
    }
}

/// Where one label is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelStatus {
    /// Not started.
    Pending,
    /// Has had at least one round and is not finished.
    Running,
    /// Its last round's apply landed everything.
    Done,
    /// Out of rounds, or a round landed nothing new.
    Incomplete,
    /// Plan or apply was refused.
    Failed,
}

/// Per-label progress in [`JobState::labels`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelProgress {
    /// The source's label.
    pub label: String,
    /// See [`LabelStatus`].
    pub status: LabelStatus,
    /// Rounds started.
    pub rounds: u32,
    /// Of those, rounds a lost source or destination volume cut short. They
    /// do not count against [`JobSpec::max_rounds`]: a drive that drops off
    /// the bus is waited for, not given up on.
    #[serde(default)]
    pub drops: u32,
    /// Items copied or cloned over all rounds.
    pub landed_items: u64,
    /// Logical bytes copied or cloned over all rounds.
    pub landed_bytes: u64,
    /// Items the last round did not land.
    pub not_landed_items: u64,
    /// The last round's plan file.
    #[serde(default)]
    pub last_plan: Option<PathBuf>,
}

/// What happened to one planned item (the `backup apply` outcomes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemOutcome {
    /// Copied and verified. Receipted.
    Copied,
    /// Cloned from a landed duplicate and verified. Receipted.
    Cloned,
    /// The drive already holds it.
    AlreadyLanded,
    /// The source changed after the plan. Skipped.
    ChangedSinceScan,
    /// The source changed while being copied. Not receipted.
    ChangedDuringCopy,
    /// Something else is at the destination path. Left alone.
    DestinationOccupied,
    /// Copy or verify failed, or the item was not attempted.
    Failed,
}

impl ItemOutcome {
    /// The item is on the drive and receipted.
    #[must_use]
    pub const fn landed(self) -> bool {
        matches!(self, Self::Copied | Self::Cloned | Self::AlreadyLanded)
    }

    /// The item was put on the drive by this round.
    #[must_use]
    pub const fn landed_now(self) -> bool {
        matches!(self, Self::Copied | Self::Cloned)
    }
}

/// Items and logical bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    /// How many.
    pub items: u64,
    /// Logical bytes.
    pub bytes: u64,
}

/// `state.json`, and the state in [`JobSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobState {
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The job's id.
    pub job_id: String,
    /// See [`Phase`].
    pub phase: Phase,
    /// The label being worked on.
    #[serde(default)]
    pub label: Option<String>,
    /// Its current round, from 1.
    pub round: u32,
    /// One entry per source, in [`JobSpec::sources`] order.
    pub labels: Vec<LabelProgress>,
    /// Outcomes over the whole job.
    pub counts: BTreeMap<ItemOutcome, Tally>,
    /// Items finished in the current round.
    pub items_done: u64,
    /// Items in the current round's plan.
    pub items_total: u64,
    /// The item being landed, relative to its source root.
    #[serde(default)]
    pub current_item: Option<String>,
    /// The last error or refusal, if any.
    #[serde(default)]
    pub last_error: Option<String>,
    /// `seq` of the last event this snapshot reflects.
    pub last_seq: u64,
    /// When a service first started the job.
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    /// When this snapshot was taken.
    pub updated_at_ms: i64,
    /// When it reached a terminal phase.
    #[serde(default)]
    pub finished_at_ms: Option<i64>,
}

impl JobState {
    /// A queued job with one pending entry per source.
    #[must_use]
    pub fn new(spec: &JobSpec, now_ms: i64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            job_id: spec.id.clone(),
            phase: Phase::Queued,
            label: None,
            round: 0,
            labels: spec
                .sources
                .iter()
                .map(|source| LabelProgress {
                    label: source.label.clone(),
                    status: LabelStatus::Pending,
                    rounds: 0,
                    drops: 0,
                    landed_items: 0,
                    landed_bytes: 0,
                    not_landed_items: 0,
                    last_plan: None,
                })
                .collect(),
            counts: BTreeMap::new(),
            items_done: 0,
            items_total: 0,
            current_item: None,
            last_error: None,
            last_seq: 0,
            started_at_ms: None,
            updated_at_ms: now_ms,
            finished_at_ms: None,
        }
    }

    /// Logical bytes copied or cloned by the job.
    #[must_use]
    pub fn landed_bytes(&self) -> u64 {
        self.labels.iter().map(|label| label.landed_bytes).sum()
    }
}

/// One line of `events.jsonl`, and one entry of [`EventsPage::events`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// From 1, one more than the previous line's.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub ts_ms: i64,
    /// What happened.
    #[serde(flatten)]
    pub body: EventBody,
}

/// The kinds of event, tagged by `"event"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventBody {
    /// The job moved to another phase.
    Phase {
        /// The new phase.
        phase: Phase,
        /// The label it concerns.
        #[serde(default)]
        label: Option<String>,
        /// That label's round.
        round: u32,
        /// Why, for waits, stops and failures.
        #[serde(default)]
        detail: Option<String>,
    },
    /// One planned item was attempted.
    Item {
        /// The source's label.
        label: String,
        /// Its round.
        round: u32,
        /// Position in the plan, from 1.
        index: u64,
        /// Items in the plan.
        total: u64,
        /// Path relative to the source root.
        relative: String,
        /// Where it lands.
        dest: String,
        /// What happened.
        outcome: ItemOutcome,
        /// Logical bytes.
        bytes: u64,
        /// Why it did not land.
        #[serde(default)]
        detail: Option<String>,
        /// A partial copy left on the drive.
        #[serde(default)]
        partial: Option<String>,
    },
    /// A round's apply finished (or stopped).
    Round {
        /// The source's label.
        label: String,
        /// Its round.
        round: u32,
        /// Copied or cloned this round.
        landed: Tally,
        /// Already on the drive.
        already_landed: Tally,
        /// Not landed (including not attempted).
        not_landed: Tally,
        /// Every item landed and every receipt was written.
        complete: bool,
        /// Why the apply stopped early, if it did.
        #[serde(default)]
        stopped: Option<String>,
    },
    /// Something failed that the job survives or that ends a label.
    Error {
        /// The label it concerns.
        #[serde(default)]
        label: Option<String>,
        /// The error, with its causes.
        message: String,
    },
}

/// `control`: what the runner should do at the next item boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// Keep going.
    #[default]
    Run,
    /// Hold between items until `run` or `cancel`.
    Pause,
    /// Stop at the next item boundary; the job becomes [`Phase::Cancelled`].
    Cancel,
}

impl Control {
    /// The file's word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Pause => "pause",
            Self::Cancel => "cancel",
        }
    }

    /// Parses the file's contents; anything unrecognised is [`Control::Run`].
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "pause" => Self::Pause,
            "cancel" => Self::Cancel,
            _ => Self::Run,
        }
    }
}

/// `run/lock`: who holds this drive's service.
///
/// A lock is stale when it names this host and either another boot or a
/// pid that is not running. A lock from another host cannot be judged and
/// is never taken over automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    /// The service's process id.
    pub pid: u32,
    /// `gethostname(3)`.
    pub host: String,
    /// The boot the pid belongs to (`kern.boottime` on macOS).
    pub boot_id: String,
    /// The service binary's version.
    pub version: String,
    /// When it took the lock.
    pub started_at_ms: i64,
}

/// `GET /v1/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// [`SCHEMA_VERSION`] the service speaks.
    pub schema_version: u32,
    /// The service binary's version.
    pub version: String,
    /// Its process id.
    pub pid: u32,
    /// Its host.
    pub host: String,
    /// The destination it serves.
    pub dest: PathBuf,
    /// The destination's physical disk (`disk14`), when known. Not an
    /// identity: macOS renumbers disks on every plug.
    #[serde(default)]
    pub dest_disk_id: Option<String>,
    /// When it started.
    pub started_at_ms: i64,
    /// Milliseconds since it started.
    pub uptime_ms: u64,
    /// The job it is running.
    #[serde(default)]
    pub active_job: Option<String>,
    /// It has been asked to stop and is finishing the current item.
    pub stopping: bool,
}

/// One job as the API (and `backup jobs`) reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
    /// `job.json`.
    pub spec: JobSpec,
    /// The live state (or `state.json` when read from the drive).
    pub state: JobState,
    /// The persisted control word.
    pub control: Control,
    /// A service is running this job right now.
    pub active: bool,
    /// [`Phase::is_unfinished`] of its phase.
    pub unfinished: bool,
}

/// `GET /v1/jobs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobList {
    /// By id.
    pub jobs: Vec<JobSummary>,
}

/// `GET /v1/jobs/{id}/events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsPage {
    /// Events with `seq > after`, in order, at most `limit`.
    pub events: Vec<Event>,
    /// The highest `seq` the job has. Ask again with
    /// `after = events.last().seq` (or `after` unchanged when empty).
    pub last_seq: u64,
    /// The job's phase is terminal: no more events will come unless it is
    /// resumed.
    pub finished: bool,
}

/// The body of every error answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// What went wrong, for a person.
    pub error: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test code")]

    use super::*;

    #[test]
    fn event_lines_are_flat_and_tagged() {
        let event = Event {
            seq: 7,
            ts_ms: 1,
            body: EventBody::Phase {
                phase: Phase::WaitingForMount,
                label: Some("mac-data".into()),
                round: 2,
                detail: None,
            },
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""seq":7"#), "{line}");
        assert!(line.contains(r#""event":"phase""#), "{line}");
        assert!(line.contains(r#""phase":"waiting_for_mount""#), "{line}");
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
    }

    #[test]
    fn counts_serialize_with_outcome_keys() {
        let spec = JobSpec {
            schema_version: SCHEMA_VERSION,
            id: "j".into(),
            created_at_ms: 0,
            dest: "/d".into(),
            sources: vec![JobSource {
                label: "a".into(),
                root: "/s".into(),
            }],
            rules: None,
            floor: 1,
            max_rounds: 1,
            rollup: true,
            catalog: None,
            mount_poll_ms: 1,
            mount_timeout_ms: None,
            profile: None,
        };
        let mut state = JobState::new(&spec, 0);
        state
            .counts
            .insert(ItemOutcome::AlreadyLanded, Tally { items: 1, bytes: 2 });
        let text = serde_json::to_string(&state).unwrap();
        assert!(text.contains(r#""already_landed":{"items":1,"bytes":2}"#), "{text}");
        assert_eq!(serde_json::from_str::<JobState>(&text).unwrap(), state);
    }

    #[test]
    fn job_ids_are_one_safe_component() {
        assert!(valid_job_id("job-1795672100145"));
        assert!(!valid_job_id(""));
        assert!(!valid_job_id(".."));
        assert!(!valid_job_id("a/b"));
        assert!(!valid_job_id(&"x".repeat(65)));
    }

    #[test]
    fn socket_keys_are_the_uuid_or_a_stable_path_hash() {
        let dest = Path::new("/Volumes/gdr22tb");
        assert_eq!(
            socket_key(Some("b6c3d4e7-1aa7-3bd1-b725-6033f3101516"), dest),
            "B6C3D4E7-1AA7-3BD1-B725-6033F3101516"
        );
        let hashed = socket_key(None, dest);
        assert!(hashed.starts_with("path-") && hashed.len() == 21, "{hashed}");
        assert_eq!(
            hashed,
            socket_key(Some("../../etc"), dest),
            "a UUID that is not hex is not trusted"
        );
        assert_ne!(hashed, socket_key(None, Path::new("/Volumes/other")));
    }

    #[test]
    fn the_socket_is_under_home_unless_that_is_too_long() {
        let key = "B6C3D4E7-1AA7-3BD1-B725-6033F3101516";
        let home = Path::new("/Users/josh");
        let tmp = Path::new("/var/folders/xy/abcdefghijklmnopqrstuvwxyz0000gn/T");
        let path = local_socket_path(Some(home), Some(tmp), key).unwrap();
        assert_eq!(
            path,
            Path::new("/Users/josh/Library/Application Support/rdirstat/run/B6C3D4E7-1AA7-3BD1-B725-6033F3101516.sock")
        );
        assert!(path.as_os_str().len() <= SUN_PATH_MAX);

        let long_home = PathBuf::from(format!("/Users/{}", "a".repeat(60)));
        let path = local_socket_path(Some(&long_home), Some(tmp), key).unwrap();
        assert!(path.starts_with(tmp.join(TMP_RUN_DIR)), "{}", path.display());

        assert_eq!(local_socket_path(Some(&long_home), None, key), None);
        assert_eq!(local_socket_path(Some(Path::new("relative")), None, key), None);
    }

    #[test]
    fn control_words_round_trip() {
        for control in [Control::Run, Control::Pause, Control::Cancel] {
            assert_eq!(Control::parse(&format!("{}\n", control.word())), control);
        }
        assert_eq!(Control::parse("garbage"), Control::Run);
    }
}
