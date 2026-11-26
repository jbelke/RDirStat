//! Backup: the app as a client of each drive's backup service.
//!
//! A drive's backup runs in `rdirstat backup serve`, a process the app
//! installs on the drive and launches detached ([`runner`]); it survives the
//! app quitting. The app talks to it over its Unix socket ([`client`]) with
//! the types in `rdirstat_jobs`. The socket is in a per-user directory keyed
//! by the drive's volume UUID, computed here exactly as the service computes
//! it ([`socket_for`]); nothing reads a socket path from the drive.
//!
//! Commands accept a destination as a string and treat it as an identifier,
//! not as authority: [`validate_dest`] resolves it and requires a mounted
//! volume's root other than `/`.
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`client`] | HTTP/1.1 + JSON over the service socket, time-bounded |
//! | [`runner`] | the sidecar: install on the drive, launch detached, run engine commands |
//! | [`drive`] | reading job records, the lock and profiles from the drive when no service answers |
//! | [`watch`] | following a job's events and forwarding them to the UI |
//!
//! # Library or sidecar
//!
//! Profile list, load and save use `rdirstat-organize` in-process: it is a
//! pure crate (TOML in and out, validation without a filesystem), so the
//! app gets typed profiles and a save that refuses an invalid one before
//! anything is written. Everything that needs the engine runs the bundled
//! sidecar instead: `backup check-profile` (paths, disks, rules),
//! `backup recommend` (walks the sources) and `backup status` (reads the
//! drive's Parquet index). Linking those would pull the scanner, planner and
//! Parquet stack into the app to duplicate code the CLI already ships, and
//! the sidecar is the very binary that runs the backup, so a check it passes
//! is the check the service will make.

mod client;
mod drive;
mod runner;
mod watch;

pub(crate) use self::watch::Watchers;

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use std::ffi::OsStr;

use hyper::Method;
use rdirstat_jobs::{
    EVENTS_DEFAULT_WAIT_SECS, EVENTS_MAX_LIMIT, EVENTS_MAX_WAIT_SECS, EventsPage, FEATURE_JOB_INCLUDES, Health,
    JobList, JobSource, JobSummary, LockInfo, NewJob, SCHEMA_VERSION, local_socket_path, socket_key, valid_job_id,
};
use rdirstat_organize::{Profile, ProfileCheck, Recommendation};
use serde::{Deserialize, Serialize};

use self::client::CallError;

/// How long a health check may take.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
/// How long any other short API call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// `backup check-profile` asks `diskutil` about each source.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
/// `backup status` reads the drive's Parquet parts.
const STATUS_TIMEOUT: Duration = Duration::from_secs(300);
/// `backup recommend` walks every source.
const RECOMMEND_TIMEOUT: Duration = Duration::from_secs(4 * 3600);
/// Most rows a recommendation may list; totals always cover every row.
const MAX_RECOMMEND_ROWS: u32 = 10_000;
/// Where mounted volumes appear.
const VOLUMES_DIR: &str = "/Volumes";

/// Why a backup command failed.
///
/// `Display` is hand-written, as for the other shell errors: the shell does
/// not depend on `thiserror`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
#[non_exhaustive]
pub(crate) enum BackupError {
    /// An argument did not validate.
    Invalid(String),
    /// The destination is not a mounted volume.
    NotMounted(String),
    /// No service answers for the drive.
    Unreachable(String),
    /// The service refused the request.
    Api {
        status: u16,
        message: String,
    },
    /// A service, job or binary of another schema.
    Incompatible(String),
    /// The service binary could not be checked, installed or started.
    Runner(String),
    Internal(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason)
            | Self::NotMounted(reason)
            | Self::Unreachable(reason)
            | Self::Incompatible(reason)
            | Self::Runner(reason)
            | Self::Internal(reason) => f.write_str(reason),
            Self::Api { status, message } => write!(f, "{message} (HTTP {status})"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<CallError> for BackupError {
    fn from(error: CallError) -> Self {
        match error {
            CallError::Api { status, message } => Self::Api { status, message },
            CallError::Unreachable(_) | CallError::TimedOut => Self::Unreachable(error.to_string()),
            CallError::Protocol(_) => Self::Internal(error.to_string()),
        }
    }
}

/// Resolves `raw` and requires the root of a mounted volume other than `/`:
/// its device differs from its parent's.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a relative path, [`BackupError::NotMounted`]
/// for anything that is not a volume root right now.
pub(crate) fn validate_dest(raw: &str) -> Result<PathBuf, BackupError> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(BackupError::Invalid(format!("{raw:?} is not an absolute path")));
    }
    let dest = std::fs::canonicalize(path).map_err(|error| BackupError::NotMounted(format!("{raw}: {error}")))?;
    let Some(parent) = dest.parent() else {
        return Err(BackupError::Invalid(
            "the boot volume is not a backup destination".to_owned(),
        ));
    };
    let device = |path: &Path| std::fs::metadata(path).map(|meta| meta.dev());
    match (device(&dest), device(parent)) {
        (Ok(own), Ok(above)) if own != above => Ok(dest),
        _ => Err(BackupError::NotMounted(format!(
            "{} is not the root of a mounted volume",
            dest.display()
        ))),
    }
}

/// Socket paths already worked out, keyed by destination and its device:
/// finding the volume UUID costs a `diskutil` call, and a remount changes
/// the device.
fn socket_cache() -> &'static Mutex<HashMap<PathBuf, (u64, PathBuf)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (u64, PathBuf)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The service socket for `dest`, as the service computes it: the volume
/// UUID under `$HOME`'s or `$TMPDIR`'s per-user run directory. Blocks (it
/// may run `diskutil`).
///
/// # Errors
///
/// [`BackupError::Internal`] when neither base gives a short enough path.
pub(crate) fn socket_for(dest: &Path) -> Result<PathBuf, BackupError> {
    let device = std::fs::metadata(dest)
        .map(|meta| meta.dev())
        .map_err(|error| BackupError::NotMounted(format!("{}: {error}", dest.display())))?;
    if let Some((cached_device, socket)) = socket_cache().lock().unwrap_or_else(PoisonError::into_inner).get(dest)
        && *cached_device == device
    {
        return Ok(socket.clone());
    }
    let uuid = rdirstat_copy::disk_identity(dest)
        .ok()
        .and_then(|identity| identity.volume_uuid);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let tmpdir = std::env::var_os("TMPDIR").map(PathBuf::from);
    let socket =
        local_socket_path(home.as_deref(), tmpdir.as_deref(), &socket_key(uuid.as_deref(), dest)).ok_or_else(|| {
            BackupError::Internal(format!(
                "no per-user directory gives a short enough socket path for {}",
                dest.display()
            ))
        })?;
    socket_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(dest.to_path_buf(), (device, socket.clone()));
    Ok(socket)
}

/// Runs blocking work off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, BackupError> + Send + 'static,
) -> Result<T, BackupError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| BackupError::Internal(format!("a background task failed: {error}")))?
}

/// `GET /v1/health` of the service at `socket`.
async fn health(socket: &Path) -> Result<Health, CallError> {
    client::call(socket, Method::GET, "/v1/health", None, HEALTH_TIMEOUT).await
}

/// A health answer this app can use: same schema, same destination.
fn compatible(health: &Health, dest: &Path) -> Result<(), BackupError> {
    if health.schema_version != SCHEMA_VERSION {
        return Err(BackupError::Incompatible(format!(
            "the backup service for {} (pid {}, version {}) speaks schema {}, this app {SCHEMA_VERSION}; \
             stop it once its job is done, then start it again from here",
            dest.display(),
            health.pid,
            health.version,
            health.schema_version
        )));
    }
    if health.dest != dest {
        return Err(BackupError::Incompatible(format!(
            "the socket for {} is answered by the service for {}",
            dest.display(),
            health.dest.display()
        )));
    }
    Ok(())
}

/// Returns the health of `dest`'s service, starting one when none answers:
/// a running, compatible service is reused as is; otherwise the bundled
/// runner is installed on the drive and launched detached ([`runner`]).
///
/// # Errors
///
/// [`BackupError::Incompatible`] for a running service of another schema
/// (it is not replaced behind its back), or whatever installing or starting
/// the runner reports.
pub(crate) async fn ensure_service(dest: PathBuf) -> Result<Health, BackupError> {
    let socket = {
        let dest = dest.clone();
        blocking(move || socket_for(&dest)).await?
    };
    let launch_dest = dest.clone();
    ensure_at(&socket, &dest, move || {
        runner::install_and_launch(&launch_dest, &runner::sidecar_path()?).map(drop)
    })
    .await
}

/// [`ensure_service`] for a known socket, with the launch step given.
async fn ensure_at(
    socket: &Path,
    dest: &Path,
    launch: impl FnOnce() -> Result<(), BackupError> + Send + 'static,
) -> Result<Health, BackupError> {
    match health(socket).await {
        Ok(health) => {
            compatible(&health, dest)?;
            return Ok(health);
        }
        Err(error) if error.is_unreachable() => {}
        Err(error) => return Err(error.into()),
    }
    blocking(launch).await?;
    let health = health(socket).await?;
    compatible(&health, dest)?;
    Ok(health)
}

/// Starts the backup service for a drive, or returns the one already
/// running there.
///
/// Installs the app's runner as `<dest>/.rdirstat/bin/rdirstat-<version>`
/// and launches it detached: it keeps running when the app quits.
///
/// # Errors
///
/// [`BackupError`]: the destination is not a mounted volume, a service of
/// another schema runs there, the drive holds a job newer than this app, or
/// the runner could not be installed or started.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_start_service(dest: String) -> Result<Health, BackupError> {
    let dest = validate_dest(&dest)?;
    ensure_service(dest).await
}

// ---------------------------------------------------------------------------
// Drives and services
// ---------------------------------------------------------------------------

/// Where a list of jobs came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobsSource {
    /// The running service: live state.
    Service,
    /// The drive's files: `state.json` may trail by a few items.
    Drive,
}

/// What runs for a drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServiceState {
    /// A compatible service answers.
    Running,
    /// A service answers, of another schema or for another drive.
    Incompatible,
    /// No answer, but the lock's holder is alive (or another host's). It
    /// may be busy, or a service from a build that listened elsewhere.
    NotAnswering,
    /// No answer, and the lock's holder is gone: it was killed or crashed.
    Stale,
    /// No answer and no lock.
    Stopped,
}

/// One mounted drive that has backup records (`<dest>/.rdirstat`).
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type)]
pub(crate) struct BackupDrive {
    /// The drive's mount point.
    pub dest: String,
    /// Its volume name.
    pub name: String,
    pub service: ServiceState,
    /// The service's answer, when one answered.
    pub health: Option<Health>,
    /// `<dest>/.rdirstat/run/lock`, when present.
    pub lock: Option<LockInfo>,
    pub jobs_source: JobsSource,
    /// Every job, by id.
    pub jobs: Vec<JobSummary>,
    /// Ids of the jobs not in a terminal phase.
    pub unfinished: Vec<String>,
    /// Profile names on the drive.
    pub profiles: Vec<String>,
}

/// Jobs of one drive.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type)]
pub(crate) struct BackupJobs {
    pub source: JobsSource,
    pub jobs: Vec<JobSummary>,
}

/// The service socket for an already validated destination.
async fn socket_of(dest: &Path) -> Result<PathBuf, BackupError> {
    let dest = dest.to_path_buf();
    blocking(move || socket_for(&dest)).await
}

/// Every volume root under `/Volumes` with a `.rdirstat` directory.
fn drives_with_records() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(VOLUMES_DIR) else {
        return Vec::new();
    };
    let mut drives: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| validate_dest(&entry.path().to_string_lossy()).ok())
        .filter(|dest| dest.join(rdirstat_jobs::RDIRSTAT_DIR).is_dir())
        .collect();
    drives.sort();
    drives.dedup();
    drives
}

/// The service's job list, or the drive's when no service answers.
async fn jobs_of(dest: &Path, socket: &Path) -> Result<BackupJobs, BackupError> {
    match client::call::<JobList>(socket, Method::GET, "/v1/jobs", None, CALL_TIMEOUT).await {
        Ok(list) => Ok(BackupJobs {
            source: JobsSource::Service,
            jobs: list.jobs,
        }),
        Err(error) if error.is_unreachable() || error == CallError::TimedOut => {
            let dest = dest.to_path_buf();
            let jobs = blocking(move || Ok(drive::read_jobs(&dest))).await?;
            Ok(BackupJobs {
                source: JobsSource::Drive,
                jobs,
            })
        }
        Err(error) => Err(error.into()),
    }
}

/// Everything [`BackupDrive`] reports for one drive.
async fn describe(dest: PathBuf) -> Result<BackupDrive, BackupError> {
    let socket = socket_of(&dest).await?;
    let answered = health(&socket).await;
    let lock_dest = dest.clone();
    let lock = blocking(move || Ok(drive::read_lock(&lock_dest))).await?;
    let (service, health) = match answered {
        Ok(health) if compatible(&health, &dest).is_ok() => (ServiceState::Running, Some(health)),
        Ok(health) => (ServiceState::Incompatible, Some(health)),
        Err(_) => {
            let state = match lock.clone() {
                None => ServiceState::Stopped,
                Some(lock) => match blocking(move || Ok(drive::holder(&lock))).await? {
                    drive::Holder::Alive => ServiceState::NotAnswering,
                    drive::Holder::Gone => ServiceState::Stale,
                },
            };
            (state, None)
        }
    };
    let jobs = if service == ServiceState::Running {
        jobs_of(&dest, &socket).await?
    } else {
        let dest = dest.clone();
        BackupJobs {
            source: JobsSource::Drive,
            jobs: blocking(move || Ok(drive::read_jobs(&dest))).await?,
        }
    };
    let profiles_dest = dest.clone();
    let profiles = blocking(move || Ok(drive::profile_names(&profiles_dest))).await?;
    Ok(BackupDrive {
        name: dest.file_name().map_or_else(
            || dest.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
        dest: dest.to_string_lossy().into_owned(),
        service,
        health,
        lock,
        jobs_source: jobs.source,
        unfinished: jobs
            .jobs
            .iter()
            .filter(|job| job.unfinished)
            .map(|job| job.spec.id.clone())
            .collect(),
        jobs: jobs.jobs,
        profiles,
    })
}

/// Every mounted drive with backup records: its service (running, stale,
/// not answering, stopped), its jobs and unfinished ones, its profiles.
///
/// Only `/Volumes/*` roots that hold a `.rdirstat` directory are listed; a
/// drive that has never been backed up to is not.
///
/// # Errors
///
/// [`BackupError::Internal`] when a background task fails. A drive that
/// cannot be described is logged and left out.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_services() -> Result<Vec<BackupDrive>, BackupError> {
    let drives = blocking(|| Ok(drives_with_records())).await?;
    let mut described = Vec::with_capacity(drives.len());
    for dest in drives {
        match describe(dest.clone()).await {
            Ok(drive) => described.push(drive),
            Err(error) => tracing::warn!(%error, dest = %dest.display(), "cannot describe a backup drive"),
        }
    }
    Ok(described)
}

/// The drive's jobs: live from its service, or from its files when no
/// service answers (`source` says which).
///
/// # Errors
///
/// [`BackupError::NotMounted`] / [`BackupError::Invalid`] for a bad
/// destination; the service's refusal otherwise.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_jobs(dest: String) -> Result<BackupJobs, BackupError> {
    let dest = validate_dest(&dest)?;
    let socket = socket_of(&dest).await?;
    jobs_of(&dest, &socket).await
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// What `backup_submit_job` may set beyond the profile.
#[derive(Clone, Debug, Default, Serialize, Deserialize, specta::Type)]
pub(crate) struct SubmitOptions {
    /// Job id. Default: `job-<created_at_ms>`.
    #[serde(default)]
    pub id: Option<String>,
    /// Rounds per source before it is called incomplete. Default 5.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// Give up waiting for a missing volume after this many ms. Default:
    /// wait until it comes back or the job is cancelled.
    #[serde(default)]
    pub mount_timeout_ms: Option<u64>,
}

/// The job for profile `name` once the sidecar resolved it: canonical roots,
/// existing includes, absolute rules path.
fn new_job(dest: &Path, name: &str, profile: &Profile, check: &ProfileCheck, options: &SubmitOptions) -> NewJob {
    let rules = profile
        .rules
        .as_ref()
        .map(|rules| drive::profiles_dir(dest).join(rules));
    NewJob {
        id: options.id.clone(),
        dest: Some(dest.to_path_buf()),
        sources: check
            .sources
            .iter()
            .map(|source| JobSource {
                label: source.label.clone(),
                root: source.root.clone(),
                include: source.includes.clone(),
            })
            .collect(),
        rules,
        floor: Some(check.floor_allocated),
        max_rounds: options.max_rounds,
        review_cap: Some(check.review_cap),
        rollup: None,
        catalog: None,
        mount_poll_ms: None,
        mount_timeout_ms: options.mount_timeout_ms,
        profile: Some(name.to_owned()),
    }
}

/// Queues a job made from profile `profile` on the drive, starting the
/// drive's service first when none runs, and starts forwarding the job's
/// events (`backup:events`).
///
/// The profile is resolved by the sidecar (`backup check-profile`) first, so
/// a source on the destination disk or a missing include is refused here,
/// not one round into the job.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a profile that does not load or resolve;
/// [`BackupError::Incompatible`] when the profile narrows a source with
/// `include` and the running service is too old to honour it; whatever
/// starting the service or the service itself reports otherwise.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_submit_job(
    app: tauri::AppHandle,
    watchers: tauri::State<'_, Watchers>,
    dest: String,
    profile: String,
    options: SubmitOptions,
) -> Result<JobSummary, BackupError> {
    let dest = validate_dest(&dest)?;
    if let Some(id) = &options.id
        && !valid_job_id(id)
    {
        return Err(BackupError::Invalid(format!("{id:?} is not a job id")));
    }
    let name = profile;
    let (loaded, check) = {
        let (dest, name) = (dest.clone(), name.clone());
        blocking(move || {
            let loaded = drive::load_profile(&dest, &name)?;
            let check = check_profile(&dest, &name).map_err(|error| BackupError::Invalid(error.to_string()))?;
            Ok((loaded, check))
        })
        .await?
    };
    if check.dest != dest {
        return Err(BackupError::Invalid(format!(
            "profile {name:?} backs up to {}, not {}",
            check.dest.display(),
            dest.display()
        )));
    }
    let health = ensure_service(dest.clone()).await?;
    let narrows = check.sources.iter().any(|source| !source.includes.is_empty());
    if narrows && !health.features.iter().any(|feature| feature == FEATURE_JOB_INCLUDES) {
        return Err(BackupError::Incompatible(format!(
            "the backup service for {} (version {}) would back up whole volumes instead of the profile's includes; \
             shut it down once its job is done and start it again from here",
            dest.display(),
            health.version
        )));
    }
    let job = new_job(&dest, &name, &loaded, &check, &options);
    let body = serde_json::to_vec(&job).map_err(|error| BackupError::Internal(error.to_string()))?;
    let socket = socket_of(&dest).await?;
    let summary: JobSummary = client::call(&socket, Method::POST, "/v1/jobs", Some(body), CALL_TIMEOUT).await?;
    watchers.watch(&app, dest, summary.spec.id.clone(), 0);
    Ok(summary)
}

/// What `backup_control` asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackupAction {
    /// Hold the job between items.
    Pause,
    /// Continue a paused job, or re-queue a stopped, cancelled, incomplete
    /// or failed one.
    Resume,
    /// Stop the job between items.
    Cancel,
    /// Finish the current item, record the job as stopped, and stop the
    /// service. The next service on the drive resumes it.
    Shutdown,
}

/// The answer to `backup_control`: the job for a job action, the service's
/// health for `shutdown`.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type)]
pub(crate) struct ControlOutcome {
    pub job: Option<JobSummary>,
    pub health: Option<Health>,
}

/// Pauses, resumes or cancels a job, or shuts the drive's service down.
/// Takes effect between items: the item being copied always finishes.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a job action without a valid job id;
/// [`BackupError::Unreachable`] when no service runs; the service's refusal
/// (`api`, e.g. 409 for pausing a finished job) otherwise.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_control(
    app: tauri::AppHandle,
    watchers: tauri::State<'_, Watchers>,
    dest: String,
    action: BackupAction,
    job: Option<String>,
) -> Result<ControlOutcome, BackupError> {
    let dest = validate_dest(&dest)?;
    let socket = socket_of(&dest).await?;
    if action == BackupAction::Shutdown {
        let health: Health = client::call(&socket, Method::POST, "/v1/shutdown", None, CALL_TIMEOUT).await?;
        return Ok(ControlOutcome {
            job: None,
            health: Some(health),
        });
    }
    let Some(job) = job.filter(|job| valid_job_id(job)) else {
        return Err(BackupError::Invalid("name a valid job id".to_owned()));
    };
    let verb = match action {
        BackupAction::Pause => "pause",
        BackupAction::Resume => "resume",
        BackupAction::Cancel | BackupAction::Shutdown => "cancel",
    };
    let summary: JobSummary = client::call(
        &socket,
        Method::POST,
        &format!("/v1/jobs/{job}/{verb}"),
        None,
        CALL_TIMEOUT,
    )
    .await?;
    if action == BackupAction::Resume {
        watchers.watch(&app, dest, job, summary.state.last_seq);
    }
    Ok(ControlOutcome {
        job: Some(summary),
        health: None,
    })
}

/// The job's events after `after`, at most `limit` (default and cap 1000).
/// A long-poll: with nothing new, waits up to `wait_secs` (default 25, at
/// most 60) for the next event. When no service answers, reads the drive's
/// `events.jsonl` at once instead; the answer is the same shape either way.
///
/// To be told about new events without polling, use `backup_watch`: it
/// forwards them as `backup:events`.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a bad job id or an unknown job.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_events(
    dest: String,
    job: String,
    after: u64,
    wait_secs: Option<u64>,
    limit: Option<u32>,
) -> Result<EventsPage, BackupError> {
    let dest = validate_dest(&dest)?;
    if !valid_job_id(&job) {
        return Err(BackupError::Invalid(format!("{job:?} is not a job id")));
    }
    let wait = wait_secs.unwrap_or(EVENTS_DEFAULT_WAIT_SECS).min(EVENTS_MAX_WAIT_SECS);
    let limit = limit
        .map_or(EVENTS_MAX_LIMIT, |limit| limit as usize)
        .clamp(1, EVENTS_MAX_LIMIT);
    let socket = socket_of(&dest).await?;
    let path = format!("/v1/jobs/{job}/events?after={after}&wait={wait}&limit={limit}");
    match client::call(&socket, Method::GET, &path, None, Duration::from_secs(wait + 10)).await {
        Ok(page) => Ok(page),
        Err(error) if error.is_unreachable() || error == CallError::TimedOut => {
            blocking(move || drive::read_events(&dest, &job, after, limit)).await
        }
        Err(CallError::Api { status: 404, message }) => Err(BackupError::Invalid(message)),
        Err(error) => Err(error.into()),
    }
}

/// Starts forwarding a job's events to the UI as `backup:events`, after
/// `after` (default: the job's current `last_seq`, i.e. only new events).
/// Idempotent. The follower survives the service going away and resumes
/// after the last `seq` it forwarded; it ends when the job is finished.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a bad job id or an unknown job.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_watch(
    app: tauri::AppHandle,
    watchers: tauri::State<'_, Watchers>,
    dest: String,
    job: String,
    after: Option<u64>,
) -> Result<bool, BackupError> {
    let dest = validate_dest(&dest)?;
    if !valid_job_id(&job) {
        return Err(BackupError::Invalid(format!("{job:?} is not a job id")));
    }
    let after = if let Some(after) = after {
        after
    } else {
        let socket = socket_of(&dest).await?;
        let jobs = jobs_of(&dest, &socket).await?;
        jobs.jobs
            .iter()
            .find(|summary| summary.spec.id == job)
            .map(|summary| summary.state.last_seq)
            .ok_or_else(|| BackupError::Invalid(format!("no job {job:?} on {}", dest.display())))?
    };
    Ok(watchers.watch(&app, dest, job, after))
}

/// Stops forwarding a job's events. Returns whether a follower was running.
///
/// # Errors
///
/// [`BackupError::Invalid`] / [`BackupError::NotMounted`] for a bad
/// destination.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_unwatch(
    watchers: tauri::State<'_, Watchers>,
    dest: String,
    job: String,
) -> Result<bool, BackupError> {
    let dest = validate_dest(&dest)?;
    Ok(watchers.unwatch(&dest, &job))
}

/// Follows every unfinished job on every mounted drive, from its current
/// `last_seq`: what the app does at start, so a backup that ran while the
/// app was closed is watched again without the user asking. It does not
/// start services; a job whose service is down is reported as
/// disconnected until one is started.
pub(crate) fn reattach(app: tauri::AppHandle) {
    use tauri::Manager as _;
    tauri::async_runtime::spawn(async move {
        let drives = match backup_services().await {
            Ok(drives) => drives,
            Err(error) => {
                tracing::warn!(%error, "cannot list backup drives; nothing re-attached");
                return;
            }
        };
        let watchers = app.state::<Watchers>();
        for drive in drives {
            for job in drive.jobs.iter().filter(|job| job.unfinished) {
                let dest = PathBuf::from(&drive.dest);
                if watchers.watch(&app, dest, job.spec.id.clone(), job.state.last_seq) {
                    tracing::info!(
                        dest = drive.dest,
                        job = job.spec.id,
                        "re-attached to an unfinished backup job"
                    );
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Profiles, recommendation, index
// ---------------------------------------------------------------------------

/// A backup profile on the wire: `rdirstat_organize::Profile` with every
/// field present (its TOML form omits defaults, which would give the
/// exported TypeScript two shapes). See that type for each field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct BackupProfile {
    /// Must be the profile format this build reads (1).
    pub schema_version: u32,
    /// Profile name; also the file stem.
    pub name: String,
    /// Destination root: the backup volume's mount point.
    pub dest: String,
    /// Rules file, relative to the profiles directory or absolute. `None`:
    /// the built-in Backup rules.
    pub rules: Option<String>,
    /// Smallest allocated size a candidate may have, in bytes.
    pub floor_allocated: u64,
    /// Review cap on ranked candidates per source.
    pub review_cap: u32,
    /// At least one.
    pub sources: Vec<BackupProfileSource>,
}

/// One source of a [`BackupProfile`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub(crate) struct BackupProfileSource {
    /// Name under `<dest>/Backups/`.
    pub label: String,
    /// Source volume root.
    pub root: String,
    /// Directories and files under `root` (relative or absolute under it).
    /// Empty: the whole root.
    pub include: Vec<String>,
}

impl From<Profile> for BackupProfile {
    fn from(profile: Profile) -> Self {
        Self {
            schema_version: profile.schema_version,
            name: profile.name,
            dest: profile.dest.to_string_lossy().into_owned(),
            rules: profile.rules.map(|rules| rules.to_string_lossy().into_owned()),
            floor_allocated: profile.floor_allocated,
            review_cap: u32::try_from(profile.review_cap).unwrap_or(u32::MAX),
            sources: profile
                .sources
                .into_iter()
                .map(|source| BackupProfileSource {
                    label: source.label,
                    root: source.root.to_string_lossy().into_owned(),
                    include: source
                        .include
                        .into_iter()
                        .map(|include| include.to_string_lossy().into_owned())
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<BackupProfile> for Profile {
    fn from(profile: BackupProfile) -> Self {
        Self {
            schema_version: profile.schema_version,
            name: profile.name,
            dest: PathBuf::from(profile.dest),
            rules: profile.rules.map(PathBuf::from),
            floor_allocated: profile.floor_allocated,
            review_cap: profile.review_cap as usize,
            sources: profile
                .sources
                .into_iter()
                .map(|source| rdirstat_organize::ProfileSource {
                    label: source.label,
                    root: PathBuf::from(source.root),
                    include: source.include.into_iter().map(PathBuf::from).collect(),
                })
                .collect(),
        }
    }
}

/// One profile file on a drive.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type)]
pub(crate) struct BackupProfileEntry {
    pub name: String,
    /// The profile, when it parses and validates.
    pub profile: Option<BackupProfile>,
    /// Why it does not.
    pub error: Option<String>,
}

/// The drive's profiles (`<dest>/.rdirstat/profiles/*.toml`), by name. A
/// file that does not parse is listed with its error, not dropped.
///
/// # Errors
///
/// [`BackupError::Invalid`] / [`BackupError::NotMounted`] for a bad
/// destination.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_profiles(dest: String) -> Result<Vec<BackupProfileEntry>, BackupError> {
    let dest = validate_dest(&dest)?;
    blocking(move || {
        Ok(drive::profile_names(&dest)
            .into_iter()
            .map(|name| match drive::load_profile(&dest, &name) {
                Ok(profile) => BackupProfileEntry {
                    name,
                    profile: Some(profile.into()),
                    error: None,
                },
                Err(error) => BackupProfileEntry {
                    name,
                    profile: None,
                    error: Some(error.to_string()),
                },
            })
            .collect())
    })
    .await
}

/// Reads profile `name` from the drive.
///
/// # Errors
///
/// [`BackupError::Invalid`] when it is missing, unreadable or invalid.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_profile_load(dest: String, name: String) -> Result<BackupProfile, BackupError> {
    let dest = validate_dest(&dest)?;
    blocking(move || drive::load_profile(&dest, &name).map(BackupProfile::from)).await
}

/// Writes a profile to `<profile.dest>/.rdirstat/profiles/<name>.toml`,
/// atomically, after the checks that need no filesystem (schema, names,
/// absolute paths, includes under their roots). Returns the path written.
/// Use `backup_profile_validate` for the checks against the disks.
///
/// # Errors
///
/// [`BackupError::NotMounted`] when `profile.dest` is not a mounted volume;
/// [`BackupError::Invalid`] when the profile does not validate (nothing is
/// written).
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_profile_save(profile: BackupProfile) -> Result<String, BackupError> {
    let profile = Profile::from(profile);
    let dest = validate_dest(&profile.dest.to_string_lossy())?;
    blocking(move || drive::save_profile(&dest, &profile).map(|path| path.to_string_lossy().into_owned())).await
}

/// `backup check-profile --json` on a saved profile.
fn check_profile(dest: &Path, name: &str) -> Result<ProfileCheck, BackupError> {
    let path = rdirstat_organize::profile_path(dest, name);
    runner::sidecar_json(
        &[
            OsStr::new("backup"),
            OsStr::new("check-profile"),
            OsStr::new("--profile"),
            path.as_os_str(),
            OsStr::new("--json"),
        ],
        CHECK_TIMEOUT,
    )
}

/// The outcome of `backup_profile_validate`.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type)]
pub(crate) struct ProfileValidation {
    pub ok: bool,
    /// The first problem, when not ok.
    pub error: Option<String>,
    /// The resolved profile, when ok.
    pub check: Option<ProfileCheck>,
}

/// Checks saved profile `name` against the filesystem, the way the service
/// will: destination and roots exist, each source is on another disk than
/// the destination, includes exist under their roots after symlinks, the
/// rules file parses. Runs the sidecar's `backup check-profile`.
///
/// # Errors
///
/// [`BackupError::Runner`] when the sidecar itself cannot run. A profile
/// that fails a check is `ok: false` with the reason, not an error.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_profile_validate(dest: String, name: String) -> Result<ProfileValidation, BackupError> {
    let dest = validate_dest(&dest)?;
    blocking(move || {
        if let Err(error) = drive::load_profile(&dest, &name) {
            return Ok(ProfileValidation {
                ok: false,
                error: Some(error.to_string()),
                check: None,
            });
        }
        runner::sidecar_path()?;
        Ok(match check_profile(&dest, &name) {
            Ok(check) => ProfileValidation {
                ok: true,
                error: None,
                check: Some(check),
            },
            Err(error) => ProfileValidation {
                ok: false,
                error: Some(error.to_string()),
                check: None,
            },
        })
    })
    .await
}

/// What a run of profile `name` would copy, defer and skip, largest first,
/// with the skip rule and reason (`backup recommend --json`). A dry run:
/// nothing is written or applied.
///
/// **Long-running**: it walks every source of the profile. `limit` (default
/// 1000, at most 10 000) caps the rows listed; totals cover every row.
///
/// # Errors
///
/// [`BackupError::Invalid`] for a profile that does not load;
/// [`BackupError::Runner`] carrying the sidecar's reason when the
/// recommendation fails (e.g. a source on the destination disk).
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_recommend(
    dest: String,
    name: String,
    limit: Option<u32>,
) -> Result<Recommendation, BackupError> {
    let dest = validate_dest(&dest)?;
    let limit = limit.unwrap_or(1000).clamp(1, MAX_RECOMMEND_ROWS).to_string();
    blocking(move || {
        drive::load_profile(&dest, &name)?;
        let path = rdirstat_organize::profile_path(&dest, &name);
        runner::sidecar_json(
            &[
                OsStr::new("backup"),
                OsStr::new("recommend"),
                OsStr::new("--profile"),
                path.as_os_str(),
                OsStr::new("--json"),
                OsStr::new("--progress"),
                OsStr::new("never"),
                OsStr::new("--limit"),
                OsStr::new(&limit),
            ],
            RECOMMEND_TIMEOUT,
        )
    })
    .await
}

/// What the drive holds per source (`backup status --json`): bytes and
/// files backed up, clone savings, pending items of the last plan, files
/// over the floor not yet backed up, and warnings. Reads the drive's
/// Parquet index only; never walks the drive. The answer is the CLI's JSON
/// document as text, unchanged: its shape belongs to `rdirstat backup
/// status --json`, and a JSON value type sends the TypeScript exporter into
/// unbounded recursion.
///
/// # Errors
///
/// [`BackupError::Runner`] carrying the sidecar's reason, e.g. a drive with
/// no index yet.
#[tauri::command]
#[specta::specta]
pub(crate) async fn backup_index_status(dest: String, source_label: Option<String>) -> Result<String, BackupError> {
    let dest = validate_dest(&dest)?;
    if let Some(label) = &source_label {
        rdirstat_organize::validate_label(label)
            .map_err(|error| BackupError::Invalid(format!("label {label:?}: {error}")))?;
    }
    blocking(move || {
        let mut args = vec![
            OsStr::new("backup"),
            OsStr::new("status"),
            OsStr::new("--dest"),
            dest.as_os_str(),
            OsStr::new("--json"),
        ];
        if let Some(label) = &source_label {
            args.push(OsStr::new("--source-label"));
            args.push(OsStr::new(label));
        }
        let doc: serde_json::Value = runner::sidecar_json(&args, STATUS_TIMEOUT)?;
        Ok(doc.to_string())
    })
    .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "test code: a failed fixture must abort loudly")]

    use super::*;

    #[test]
    fn a_destination_must_be_a_mounted_volume_root() {
        assert!(matches!(validate_dest("relative/path"), Err(BackupError::Invalid(_))));
        assert!(matches!(validate_dest("/"), Err(BackupError::Invalid(_))));
        let dir = tempfile::tempdir().expect("tempdir");
        let plain = dir.path().to_string_lossy().into_owned();
        assert!(matches!(validate_dest(&plain), Err(BackupError::NotMounted(_))));
        assert!(matches!(
            validate_dest("/no/such/volume"),
            Err(BackupError::NotMounted(_))
        ));
    }

    #[test]
    fn a_service_of_another_schema_or_drive_is_not_used() {
        let dest = Path::new("/Volumes/b");
        let mut health = Health {
            schema_version: SCHEMA_VERSION,
            version: "x".into(),
            pid: 1,
            host: "h".into(),
            dest: dest.to_path_buf(),
            dest_disk_id: None,
            started_at_ms: 0,
            uptime_ms: 0,
            active_job: None,
            stopping: false,
            features: Vec::new(),
        };
        compatible(&health, dest).expect("same schema and drive");
        health.dest = PathBuf::from("/Volumes/other");
        assert!(matches!(compatible(&health, dest), Err(BackupError::Incompatible(_))));
        health.dest = dest.to_path_buf();
        health.schema_version = SCHEMA_VERSION + 1;
        assert!(matches!(compatible(&health, dest), Err(BackupError::Incompatible(_))));
    }

    #[test]
    fn a_job_from_a_profile_carries_its_resolved_sources_and_absolute_rules() {
        let dest = Path::new("/Volumes/b");
        let profile = Profile {
            schema_version: rdirstat_organize::PROFILE_SCHEMA_VERSION,
            name: "laptop".into(),
            dest: dest.to_path_buf(),
            rules: Some("rules.toml".into()),
            floor_allocated: 7,
            review_cap: 9,
            sources: Vec::new(),
        };
        let check = ProfileCheck {
            name: "laptop".into(),
            dest: dest.to_path_buf(),
            floor_allocated: 7,
            review_cap: 9,
            sources: vec![rdirstat_organize::ProfileCheckSource {
                label: "mac-data".into(),
                root: "/System/Volumes/Data".into(),
                includes: vec!["Users/x/Documents".into()],
            }],
        };
        let options = SubmitOptions {
            id: Some("job-1".into()),
            max_rounds: Some(3),
            mount_timeout_ms: None,
        };
        let job = new_job(dest, "laptop", &profile, &check, &options);
        let wire = BackupProfile::from(profile.clone());
        assert_eq!(wire.rules.as_deref(), Some("rules.toml"));
        assert_eq!(Profile::from(wire), profile, "the wire form round-trips");
        assert_eq!(job.id.as_deref(), Some("job-1"));
        assert_eq!(job.dest.as_deref(), Some(dest));
        assert_eq!(
            job.rules,
            Some(PathBuf::from("/Volumes/b/.rdirstat/profiles/rules.toml"))
        );
        assert_eq!((job.floor, job.review_cap, job.max_rounds), (Some(7), Some(9), Some(3)));
        assert_eq!(job.profile.as_deref(), Some("laptop"));
        assert_eq!(job.sources.len(), 1);
        assert_eq!(job.sources[0].root, PathBuf::from("/System/Volumes/Data"));
        assert_eq!(job.sources[0].include, vec![PathBuf::from("Users/x/Documents")]);
    }

    fn health_answer(dest: &Path, pid: u32) -> String {
        serde_json::json!({
            "schema_version": SCHEMA_VERSION, "version": "0.1.0", "pid": pid, "host": "h",
            "dest": dest, "started_at_ms": 0, "uptime_ms": 5, "stopping": false
        })
        .to_string()
    }

    #[test]
    fn a_running_compatible_service_is_reused_without_launching() {
        let dir = client::tests::short_dir();
        let socket = dir.path().join("s.sock");
        let dest = PathBuf::from("/Volumes/b");
        let server = client::tests::fake_service(&socket, vec![(200, health_answer(&dest, 7))]);
        let found = tauri::async_runtime::block_on(ensure_at(&socket, &dest, || {
            Err(BackupError::Internal("must not launch".into()))
        }))
        .expect("reused");
        assert_eq!(found.pid, 7);
        assert_eq!(server.join().expect("server"), vec!["GET /v1/health HTTP/1.1"]);
    }

    #[test]
    fn with_no_service_the_runner_is_launched_and_then_asked() {
        let dir = client::tests::short_dir();
        let socket = dir.path().join("s.sock");
        let dest = PathBuf::from("/Volumes/b");
        let launched = std::sync::Arc::new(Mutex::new(None));
        let slot = std::sync::Arc::clone(&launched);
        let (launch_socket, launch_dest) = (socket.clone(), dest.clone());
        let found = tauri::async_runtime::block_on(ensure_at(&socket, &dest, move || {
            // Stands in for `serve --detach`: the service is up when it returns.
            *slot.lock().expect("lock") = Some(client::tests::fake_service(
                &launch_socket,
                vec![(200, health_answer(&launch_dest, 9))],
            ));
            Ok(())
        }))
        .expect("launched");
        assert_eq!(found.pid, 9);
        let server = launched.lock().expect("lock").take().expect("launch ran");
        assert_eq!(server.join().expect("server"), vec!["GET /v1/health HTTP/1.1"]);
    }
}
