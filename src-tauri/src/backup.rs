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

mod client;
mod runner;

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use hyper::Method;
use rdirstat_jobs::{Health, SCHEMA_VERSION, local_socket_path, socket_key};
use serde::{Deserialize, Serialize};

use self::client::CallError;

/// How long a health check may take.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

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
        };
        compatible(&health, dest).expect("same schema and drive");
        health.dest = PathBuf::from("/Volumes/other");
        assert!(matches!(compatible(&health, dest), Err(BackupError::Incompatible(_))));
        health.dest = dest.to_path_buf();
        health.schema_version = SCHEMA_VERSION + 1;
        assert!(matches!(compatible(&health, dest), Err(BackupError::Incompatible(_))));
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
