//! `rdirstat backup serve`: the drive-resident backup service.
//!
//! One service per destination drive. It holds `<dest>/.rdirstat/run/lock`,
//! answers the `/v1` API (see `rdirstat_jobs`) as HTTP/1.1 over a Unix
//! socket in a per-user directory on the boot disk (mode 0600 in a 0700
//! directory the current user owns, keyed by the drive's volume UUID; see
//! [`rdirstat_jobs::local_socket_path`]; no TCP listener), and runs jobs one
//! at a time on a worker thread. The socket is not on the drive because a
//! drive mounted with ownership ignored does not enforce its mode. Every
//! job's events and state are on the drive, so a job the previous service
//! did not finish is picked up again when the next one starts, and
//! `backup jobs` can read them while no service runs.
//!
//! The handlers in [`http`] take a parsed request and return a response;
//! they know nothing of the socket, so another listener (TCP + TLS) can be
//! added beside the Unix one without touching them.

pub(crate) mod client;
mod http;
pub(crate) mod lock;
mod record;
mod runner;
mod sys;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use rdirstat_jobs::{
    Control, DEFAULT_FLOOR, DEFAULT_MAX_ROUNDS, DEFAULT_MOUNT_POLL_MS, EventBody, Health, JOB_FILE, JobList, JobSpec,
    JobState, JobSummary, LOCK_FILE, LabelStatus, NewJob, Phase, SCHEMA_VERSION, SUN_PATH_MAX, job_dir, jobs_dir,
    local_socket_path, run_dir, socket_key, valid_job_id,
};
use tokio::sync::watch;

use rdirstat_jobs::LockInfo;

use self::lock::Lock;
use self::record::Record;
pub(crate) use self::runner::ItemHook;
use self::runner::Runner;
pub(crate) use self::sys::new_session;
use super::{DiskProbe, now_ms};

/// How often the listener checks that its socket is still at its path (it
/// is not after something removes it, such as a temp-dir cleaner).
const REBIND_EVERY: Duration = Duration::from_secs(5);

/// The per-user directories a socket path is derived from.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalDirs {
    pub(crate) home: Option<PathBuf>,
    pub(crate) tmpdir: Option<PathBuf>,
}

impl LocalDirs {
    /// `$HOME` and `$TMPDIR`.
    pub(crate) fn from_env() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            tmpdir: std::env::var_os("TMPDIR").map(PathBuf::from),
        }
    }
}

/// The API socket of the service for `dest`, computed the same way by the
/// service and by every client: the destination's volume UUID (from
/// `probe`) under the per-user run directory. Never a path on the drive.
///
/// # Errors
///
/// Neither `$HOME` nor `$TMPDIR` gives a socket path short enough to bind.
pub(crate) fn socket_for(dest: &Path, probe: &dyn DiskProbe, dirs: &LocalDirs) -> Result<PathBuf> {
    let uuid = probe.facts(dest).ok().and_then(|facts| facts.volume_uuid);
    let key = socket_key(uuid.as_deref(), dest);
    local_socket_path(dirs.home.as_deref(), dirs.tmpdir.as_deref(), &key).with_context(|| {
        format!(
            "no per-user directory gives a socket path of at most {SUN_PATH_MAX} bytes for {} (HOME {:?}, TMPDIR {:?})",
            dest.display(),
            dirs.home,
            dirs.tmpdir
        )
    })
}

/// Creates the socket's directory with mode 0700 (and any missing parent
/// with 0700), tightens an existing one to 0700, and refuses it unless the
/// current user owns it, so no other local user can reach the socket.
fn prepare_socket_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.uid() != sys::euid() {
        bail!(
            "{} belongs to uid {}, not to this user (uid {}); refusing to put the API socket there",
            dir.display(),
            meta.uid(),
            sys::euid()
        );
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", dir.display()))
}

/// What `serve` was asked.
pub(crate) struct ServeConfig {
    pub(crate) dest: PathBuf,
    pub(crate) probe: Arc<dyn DiskProbe + Send + Sync>,
    /// Catalog for jobs whose spec names none.
    pub(crate) default_catalog: Option<PathBuf>,
    pub(crate) app_catalog: Option<PathBuf>,
    /// Where the API socket goes (see [`socket_for`]).
    pub(crate) local: LocalDirs,
    /// Stop on `SIGTERM` and `SIGINT`. Off in tests, which share a process.
    pub(crate) signals: bool,
    pub(crate) on_item: Option<Arc<ItemHook>>,
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("dest", &self.dest)
            .finish_non_exhaustive()
    }
}

/// A job the service knows.
#[derive(Debug)]
struct JobHandle {
    spec: JobSpec,
    record: Mutex<Record>,
}

impl JobHandle {
    fn record(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The running service, shared by the worker and every connection.
pub(crate) struct Service {
    dest: PathBuf,
    holder: LockInfo,
    started: Instant,
    dest_disk_id: Option<String>,
    jobs: Mutex<BTreeMap<String, Arc<JobHandle>>>,
    queue: Mutex<VecDeque<String>>,
    wake: Condvar,
    active: Mutex<Option<String>>,
    shutdown: AtomicBool,
    stopping: watch::Sender<bool>,
    config: ServeConfig,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("dest", &self.dest)
            .finish_non_exhaustive()
    }
}

/// A refused API call: an HTTP status and a message.
#[derive(Debug)]
pub(crate) struct ApiFailure {
    pub(crate) status: u16,
    pub(crate) message: String,
}

impl ApiFailure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

type ApiResult<T> = std::result::Result<T, ApiFailure>;

/// What the API can ask of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Pause,
    Resume,
    Cancel,
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Service {
    fn job(&self, id: &str) -> ApiResult<Arc<JobHandle>> {
        lock_mutex(&self.jobs)
            .get(id)
            .cloned()
            .ok_or_else(|| ApiFailure::new(404, format!("no job {id:?}")))
    }

    fn active_job(&self) -> Option<String> {
        lock_mutex(&self.active).clone()
    }

    /// `GET /v1/health`.
    pub(crate) fn health(&self) -> Health {
        Health {
            schema_version: SCHEMA_VERSION,
            version: self.holder.version.clone(),
            pid: self.holder.pid,
            host: self.holder.host.clone(),
            dest: self.dest.clone(),
            dest_disk_id: self.dest_disk_id.clone(),
            started_at_ms: self.holder.started_at_ms,
            uptime_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            active_job: self.active_job(),
            stopping: self.shutdown.load(Ordering::SeqCst),
            features: vec![rdirstat_jobs::FEATURE_JOB_INCLUDES.to_owned()],
        }
    }

    fn summary(&self, handle: &JobHandle) -> JobSummary {
        let record = handle.record();
        let state = record.state().clone();
        JobSummary {
            spec: handle.spec.clone(),
            unfinished: state.phase.is_unfinished(),
            state,
            control: record.control(),
            active: self.active_job().as_deref() == Some(handle.spec.id.as_str()),
        }
    }

    /// `GET /v1/jobs`.
    pub(crate) fn list(&self) -> JobList {
        let handles: Vec<_> = lock_mutex(&self.jobs).values().cloned().collect();
        JobList {
            jobs: handles.iter().map(|handle| self.summary(handle)).collect(),
        }
    }

    /// `GET /v1/jobs/{id}`.
    pub(crate) fn get(&self, id: &str) -> ApiResult<JobSummary> {
        let handle = self.job(id)?;
        Ok(self.summary(&handle))
    }

    /// `POST /v1/jobs`: writes the job's directory and queues it.
    pub(crate) fn create(&self, new: NewJob) -> ApiResult<JobSummary> {
        if self.shutdown.load(Ordering::SeqCst) {
            return Err(ApiFailure::new(503, "the service is shutting down"));
        }
        let spec = self.spec_for(new)?;
        let dir = job_dir(&self.dest, &spec.id);
        let jobs = jobs_dir(&self.dest);
        if !self.dest.join(rdirstat_jobs::RDIRSTAT_DIR).is_dir() {
            return Err(ApiFailure::new(503, format!("{} is not mounted", self.dest.display())));
        }
        std::fs::create_dir_all(&jobs)
            .and_then(|()| std::fs::create_dir(&dir))
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::AlreadyExists => ApiFailure::new(409, format!("job {:?} exists", spec.id)),
                _ => ApiFailure::new(500, format!("cannot create {}: {error}", dir.display())),
            })?;
        record::write_atomic(&dir, JOB_FILE, &spec)
            .map_err(|error| ApiFailure::new(500, format!("cannot write {}: {error}", dir.join(JOB_FILE).display())))?;
        let mut record = Record::load(&dir, &spec).map_err(|error| ApiFailure::new(500, format!("{error:#}")))?;
        record.set_control(Control::Run);
        record.emit(EventBody::Phase {
            phase: Phase::Queued,
            label: None,
            round: 0,
            detail: None,
        });
        let handle = Arc::new(JobHandle {
            spec,
            record: Mutex::new(record),
        });
        lock_mutex(&self.jobs).insert(handle.spec.id.clone(), Arc::clone(&handle));
        self.enqueue(&handle.spec.id);
        Ok(self.summary(&handle))
    }

    fn spec_for(&self, new: NewJob) -> ApiResult<JobSpec> {
        let created_at_ms = now_ms();
        if let Some(dest) = &new.dest {
            let same = std::fs::canonicalize(dest).is_ok_and(|dest| dest == self.dest);
            if !same {
                return Err(ApiFailure::new(
                    400,
                    format!(
                        "this service backs up to {}, not {}",
                        self.dest.display(),
                        dest.display()
                    ),
                ));
            }
        }
        if new.sources.is_empty() {
            return Err(ApiFailure::new(400, "a job needs at least one source"));
        }
        if let Some(cap) = new.review_cap
            && (cap == 0 || cap > rdirstat_organize::profile::MAX_REVIEW_CAP)
        {
            return Err(ApiFailure::new(
                400,
                format!(
                    "review_cap {cap} must be between 1 and {}",
                    rdirstat_organize::profile::MAX_REVIEW_CAP
                ),
            ));
        }
        let mut sources = new.sources;
        for source in &mut sources {
            let mut includes = Vec::with_capacity(source.include.len());
            for include in &source.include {
                let relative = if include.is_absolute() {
                    None
                } else {
                    rdirstat_organize::profile::include_relative(&source.root, include)
                };
                let Some(relative) = relative else {
                    return Err(ApiFailure::new(
                        400,
                        format!(
                            "include {} of {:?} is not a relative path under its root",
                            include.display(),
                            source.label
                        ),
                    ));
                };
                includes.push(relative);
            }
            source.include = includes;
        }
        let mut labels = std::collections::HashSet::new();
        for source in &sources {
            rdirstat_organize::validate_label(&source.label)
                .map_err(|error| ApiFailure::new(400, format!("label {:?}: {error}", source.label)))?;
            if !labels.insert(source.label.as_str()) {
                return Err(ApiFailure::new(400, format!("label {:?} is given twice", source.label)));
            }
            if !source.root.is_absolute() {
                return Err(ApiFailure::new(
                    400,
                    format!("source root {} is not absolute", source.root.display()),
                ));
            }
        }
        let id = new.id.unwrap_or_else(|| format!("job-{created_at_ms}"));
        if !valid_job_id(&id) {
            return Err(ApiFailure::new(400, format!("{id:?} is not a job id")));
        }
        if lock_mutex(&self.jobs).contains_key(&id) {
            return Err(ApiFailure::new(409, format!("job {id:?} exists")));
        }
        Ok(JobSpec {
            schema_version: SCHEMA_VERSION,
            id,
            created_at_ms,
            dest: self.dest.clone(),
            sources,
            rules: new.rules,
            floor: new.floor.unwrap_or(DEFAULT_FLOOR),
            max_rounds: new.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS).max(1),
            review_cap: new.review_cap,
            rollup: new.rollup.unwrap_or(true),
            catalog: new.catalog,
            mount_poll_ms: new.mount_poll_ms.unwrap_or(DEFAULT_MOUNT_POLL_MS).max(1),
            mount_timeout_ms: new.mount_timeout_ms,
            profile: new.profile,
        })
    }

    fn enqueue(&self, id: &str) {
        let mut queue = lock_mutex(&self.queue);
        if !queue.iter().any(|queued| queued == id) {
            queue.push_back(id.to_owned());
        }
        self.wake.notify_all();
    }

    /// `POST /v1/jobs/{id}/pause|resume|cancel`.
    pub(crate) fn act(&self, id: &str, action: Action) -> ApiResult<JobSummary> {
        let handle = self.job(id)?;
        let active = self.active_job().as_deref() == Some(id);
        let queued = lock_mutex(&self.queue).iter().any(|queued| queued == id);
        {
            let mut record = handle.record();
            let phase = record.state().phase;
            match action {
                Action::Pause => {
                    if phase.is_terminal() {
                        return Err(ApiFailure::new(
                            409,
                            format!("job {id:?} is {phase:?}; nothing to pause"),
                        ));
                    }
                    record.set_control(Control::Pause);
                }
                Action::Cancel => {
                    if phase.is_terminal() && phase != Phase::Incomplete && phase != Phase::Failed {
                        return Err(ApiFailure::new(409, format!("job {id:?} is already {phase:?}")));
                    }
                    record.set_control(Control::Cancel);
                    if !active {
                        lock_mutex(&self.queue).retain(|queued| queued != id);
                        let state = record.state_mut();
                        state.phase = Phase::Cancelled;
                        state.finished_at_ms = Some(now_ms());
                        let (label, round) = (state.label.clone(), state.round);
                        record.emit(EventBody::Phase {
                            phase: Phase::Cancelled,
                            label,
                            round,
                            detail: Some("cancelled".into()),
                        });
                    }
                }
                Action::Resume => {
                    record.set_control(Control::Run);
                    if !active && !queued {
                        let state = record.state_mut();
                        for label in &mut state.labels {
                            if matches!(label.status, LabelStatus::Incomplete | LabelStatus::Failed) {
                                label.status = LabelStatus::Pending;
                                label.rounds = 0;
                                label.drops = 0;
                            }
                        }
                        state.phase = Phase::Queued;
                        state.finished_at_ms = None;
                        let (label, round) = (state.label.clone(), state.round);
                        record.emit(EventBody::Phase {
                            phase: Phase::Queued,
                            label,
                            round,
                            detail: Some("resumed".into()),
                        });
                    }
                }
            }
        }
        if action == Action::Resume && !active && !queued {
            self.enqueue(id);
        }
        Ok(self.summary(&handle))
    }

    /// The pieces `GET /v1/jobs/{id}/events` needs to long-poll.
    pub(crate) fn events_source(&self, id: &str) -> ApiResult<EventsSource> {
        let handle = self.job(id)?;
        let seq = handle.record().subscribe();
        Ok(EventsSource {
            handle,
            seq,
            stopping: self.stopping.subscribe(),
        })
    }

    /// `POST /v1/shutdown` and `SIGTERM`: the current item finishes, the job
    /// is recorded as stopped, then the service exits.
    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _guard = lock_mutex(&self.queue);
        self.wake.notify_all();
        self.stopping.send_replace(true);
    }

    fn worker(&self) {
        loop {
            let id = {
                let mut queue = lock_mutex(&self.queue);
                loop {
                    if self.shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Some(id) = queue.pop_front() {
                        break id;
                    }
                    queue = self
                        .wake
                        .wait_timeout(queue, Duration::from_secs(1))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            };
            let Ok(handle) = self.job(&id) else { continue };
            *lock_mutex(&self.active) = Some(id.clone());
            let runner = Runner {
                spec: &handle.spec,
                record: &handle.record,
                probe: self.config.probe.as_ref(),
                shutdown: &self.shutdown,
                default_catalog: self.config.default_catalog.clone(),
                app_catalog: self.config.app_catalog.clone(),
                on_item: self.config.on_item.as_deref(),
            };
            let phase = runner.run();
            *lock_mutex(&self.active) = None;
            eprintln!("rdirstat: job {id} ended {phase:?}");
        }
    }
}

/// A job's event stream for one long-poll.
pub(crate) struct EventsSource {
    handle: Arc<JobHandle>,
    pub(crate) seq: watch::Receiver<u64>,
    pub(crate) stopping: watch::Receiver<bool>,
}

impl std::fmt::Debug for EventsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventsSource")
            .field("job", &self.handle.spec.id)
            .finish_non_exhaustive()
    }
}

impl EventsSource {
    /// Events after `after` (at most `limit`), the highest seq, and whether
    /// the job is in a terminal phase.
    pub(crate) fn page(&self, after: u64, limit: usize) -> ApiResult<rdirstat_jobs::EventsPage> {
        let record = self.handle.record();
        let events = record
            .events_after(after, limit)
            .map_err(|error| ApiFailure::new(503, format!("{error:#}")))?;
        Ok(rdirstat_jobs::EventsPage {
            events,
            last_seq: record.last_seq(),
            finished: record.state().phase.is_terminal(),
        })
    }
}

/// A started service: the API thread and the worker thread.
#[derive(Debug)]
pub(crate) struct Running {
    pub(crate) service: Arc<Service>,
    pub(crate) socket: PathBuf,
    thread: std::thread::JoinHandle<Result<()>>,
}

impl Running {
    /// Asks the service to stop (see [`Service::request_shutdown`]).
    #[cfg(test)]
    pub(crate) fn shutdown(&self) {
        self.service.request_shutdown();
    }

    /// Waits for the service to exit.
    ///
    /// # Errors
    ///
    /// The API loop failed.
    pub(crate) fn join(self) -> Result<()> {
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => bail!("the backup service thread panicked"),
        }
    }
}

/// Starts the service for `config.dest`: takes the lock (taking over a stale
/// one), replaces a stale socket in the per-user run directory, loads every job on the drive, queues the
/// unfinished ones, and serves until shut down.
///
/// # Errors
///
/// The destination does not resolve, the lock is held by a live service
/// (or cannot be judged), the socket directory is not this user's, the
/// socket cannot be bound, or a job on the drive
/// cannot be read.
pub(crate) fn start(config: ServeConfig) -> Result<Running> {
    let dest =
        std::fs::canonicalize(&config.dest).with_context(|| format!("cannot resolve {}", config.dest.display()))?;
    let run = run_dir(&dest);
    std::fs::create_dir_all(&run).with_context(|| format!("cannot create {}", run.display()))?;
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", run.display()))?;
    let lock = Lock::acquire(&run.join(LOCK_FILE), lock::me(now_ms()))?;
    if let Some(stale) = &lock.took_over {
        eprintln!(
            "rdirstat: took over a stale lock (pid {} on {}, version {})",
            stale.pid, stale.host, stale.version
        );
    }
    let socket = socket_for(&dest, config.probe.as_ref(), &config.local)?;
    let listener = bind(&socket)?;
    let dest_disk_id = config.probe.facts(&dest).ok().and_then(|facts| facts.disk_id);
    let (stopping, _) = watch::channel(false);
    let service = Arc::new(Service {
        holder: lock.info().clone(),
        started: Instant::now(),
        dest_disk_id,
        jobs: Mutex::new(BTreeMap::new()),
        queue: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
        active: Mutex::new(None),
        shutdown: AtomicBool::new(false),
        stopping,
        dest: dest.clone(),
        config,
    });
    load_jobs(&service)?;
    let thread_service = Arc::clone(&service);
    let thread_socket = socket.clone();
    let thread = std::thread::Builder::new()
        .name("rdirstat-serve".into())
        .spawn(move || {
            let result = serve_loop(&thread_service, listener, &thread_socket);
            remove_if_ours(&thread_socket, &result);
            drop(lock);
            result.map(|_| ())
        })
        .context("cannot start the service thread")?;
    Ok(Running {
        service,
        socket,
        thread,
    })
}

/// Binds the socket, replacing whatever is at its path (the caller holds the
/// lock, so it is a dead service's), and restricts it to the owner. The
/// directory is created or tightened first ([`prepare_socket_dir`]).
fn bind(socket: &Path) -> Result<std::os::unix::net::UnixListener> {
    if socket.as_os_str().len() > SUN_PATH_MAX {
        bail!(
            "{} is longer than a Unix socket path may be ({SUN_PATH_MAX} bytes)",
            socket.display()
        );
    }
    let Some(dir) = socket.parent() else {
        bail!("{} has no directory", socket.display());
    };
    prepare_socket_dir(dir)?;
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("cannot remove the stale {}", socket.display())),
    }
    let listener = std::os::unix::net::UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict {}", socket.display()))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Removes the socket if it is still the one this service bound.
fn remove_if_ours(socket: &Path, bound: &Result<u64>) {
    if let Ok(ino) = bound
        && std::fs::symlink_metadata(socket).is_ok_and(|meta| meta.ino() == *ino)
    {
        let _ignored = std::fs::remove_file(socket);
    }
}

/// Reads every job directory on the drive and queues the unfinished jobs,
/// oldest first.
fn load_jobs(service: &Service) -> Result<()> {
    let dir = jobs_dir(&service.dest);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut unfinished = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let spec_path = path.join(JOB_FILE);
        if !spec_path.is_file() {
            continue;
        }
        let spec: JobSpec = match std::fs::read(&spec_path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(anyhow::Error::from))
        {
            Ok(spec) => spec,
            Err(error) => {
                eprintln!("rdirstat: skipping {}: {error:#}", spec_path.display());
                continue;
            }
        };
        let record = Record::load(&path, &spec)?;
        let phase = record.state().phase;
        if phase.is_unfinished() {
            unfinished.push((spec.created_at_ms, spec.id.clone()));
        }
        let handle = Arc::new(JobHandle {
            spec,
            record: Mutex::new(record),
        });
        lock_mutex(&service.jobs).insert(handle.spec.id.clone(), handle);
    }
    unfinished.sort();
    for (_, id) in unfinished {
        service.enqueue(&id);
    }
    Ok(())
}

/// The API loop: accepts connections until the worker has finished after a
/// shutdown request. Returns the inode of the socket it last bound.
fn serve_loop(service: &Arc<Service>, listener: std::os::unix::net::UnixListener, socket: &Path) -> Result<u64> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let worker_service = Arc::clone(service);
    let worker = std::thread::Builder::new()
        .name("rdirstat-jobs".into())
        .spawn(move || worker_service.worker())
        .context("cannot start the job worker")?;
    let result = runtime.block_on(accept_loop(Arc::clone(service), listener, socket, worker));
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn accept_loop(
    service: Arc<Service>,
    listener: std::os::unix::net::UnixListener,
    socket: &Path,
    worker: std::thread::JoinHandle<()>,
) -> Result<u64> {
    let mut ino = std::fs::symlink_metadata(socket).map_or(0, |meta| meta.ino());
    let mut listener = tokio::net::UnixListener::from_std(listener)?;
    if service.config.signals {
        spawn_signal_handlers(&service)?;
    }
    let mut stopping = service.stopping.subscribe();
    let mut tick = tokio::time::interval(REBIND_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let worker_done = {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ignored = worker.join();
            let _ignored = tx.send(());
        });
        rx
    };
    tokio::pin!(worker_done);
    let mut draining = false;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let service = Arc::clone(&service);
                    tokio::spawn(http::serve_connection(service, stream));
                }
            }
            _ = tick.tick() => {
                if let Some(rebound) = rebind_if_gone(socket, ino) {
                    match rebound.and_then(|std| Ok((std::fs::symlink_metadata(socket)?.ino(), tokio::net::UnixListener::from_std(std)?))) {
                        Ok((new_ino, new_listener)) => {
                            eprintln!("rdirstat: {} was gone; listening again", socket.display());
                            listener = new_listener;
                            ino = new_ino;
                        }
                        Err(error) => eprintln!("rdirstat: cannot listen on {} again: {error:#}", socket.display()),
                    }
                }
            }
            changed = stopping.changed(), if !draining => {
                if changed.is_err() || *stopping.borrow() {
                    draining = true;
                }
            }
            _ = &mut worker_done => break,
        }
    }
    Ok(ino)
}

/// `Some(new listener)` when the socket at `path` is no longer the one with
/// inode `ino`; `None` otherwise. The socket is on the boot disk, so it is
/// rebound (and its directory recreated) whether or not the drive is there.
fn rebind_if_gone(socket: &Path, ino: u64) -> Option<Result<std::os::unix::net::UnixListener>> {
    if std::fs::symlink_metadata(socket).is_ok_and(|meta| meta.ino() == ino) {
        return None;
    }
    Some(bind(socket))
}

fn spawn_signal_handlers(service: &Arc<Service>) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    for kind in [SignalKind::terminate(), SignalKind::interrupt(), SignalKind::hangup()] {
        let mut stream = signal(kind).context("cannot install a signal handler")?;
        let service = Arc::clone(service);
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                eprintln!("rdirstat: signal received; finishing the current item");
                service.request_shutdown();
            }
        });
    }
    Ok(())
}

/// What `backup jobs` prints for one job when the service is down: read
/// straight from the drive.
///
/// # Errors
///
/// The jobs directory exists but cannot be read.
pub(crate) fn read_jobs(dest: &Path) -> Result<JobList> {
    let dir = jobs_dir(dest);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(JobList { jobs: Vec::new() }),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut jobs = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let Ok(bytes) = std::fs::read(path.join(JOB_FILE)) else {
            continue;
        };
        let Ok(spec) = serde_json::from_slice::<JobSpec>(&bytes) else {
            continue;
        };
        let state = std::fs::read(path.join(rdirstat_jobs::STATE_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<JobState>(&bytes).ok())
            .unwrap_or_else(|| JobState::new(&spec, spec.created_at_ms));
        let control = std::fs::read_to_string(path.join(rdirstat_jobs::CONTROL_FILE))
            .map_or(Control::Run, |text| Control::parse(&text));
        jobs.push(JobSummary {
            unfinished: state.phase.is_unfinished(),
            spec,
            state,
            control,
            active: false,
        });
    }
    jobs.sort_by(|a, b| a.spec.id.cmp(&b.spec.id));
    Ok(JobList { jobs })
}

/// `backup jobs` as text: one line per job, unfinished ones flagged.
pub(crate) fn jobs_text(dest: &Path, list: &JobList, source: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let lock = lock::read(&run_dir(dest).join(LOCK_FILE));
    let service = match (&lock, source) {
        (_, "service") => "running".to_owned(),
        (Ok(Some(info)), _) if lock::alive(info) => format!("pid {} holds the lock but does not answer", info.pid),
        (Ok(Some(info)), _) => format!("not running (stale lock from pid {})", info.pid),
        (Ok(None), _) => "not running".to_owned(),
        (Err(why), _) => why.clone(),
    };
    let _ = writeln!(out, "Backup jobs on {} (service: {service})", dest.display());
    if list.jobs.is_empty() {
        out.push_str("  none\n");
        return out;
    }
    for job in &list.jobs {
        let state = &job.state;
        let flag = if job.active {
            "  RUNNING"
        } else if job.unfinished {
            "  UNFINISHED"
        } else {
            ""
        };
        let labels: Vec<String> = state
            .labels
            .iter()
            .map(|label| format!("{} {:?} r{}", label.label, label.status, label.rounds))
            .collect();
        let _ = writeln!(
            out,
            "  {}  {:?}  landed {}  [{}]  control {}{flag}",
            job.spec.id,
            state.phase,
            rdirstat_core::format_iec(state.landed_bytes()),
            labels.join(", "),
            job.control.word()
        );
        if let Some(error) = &state.last_error {
            let _ = writeln!(out, "    last error: {error}");
        }
    }
    out
}
