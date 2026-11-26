//! Installing the service binary on a drive and launching it detached.
//!
//! The app ships the `rdirstat` CLI as the Tauri sidecar `rdirstat-runner`
//! (`bundle.externalBin`; staged by `scripts/build-sidecar.mjs`). To run a
//! drive's backup service it:
//!
//! 1. asks the sidecar `backup version` and refuses one that is not this
//!    app's version and job schema (a placeholder from a plain `cargo build`
//!    fails here, with its instructions);
//! 2. refuses when any `job.json` on the drive has a newer schema than the
//!    binary speaks (a newer app wrote it);
//! 3. copies the sidecar to `<dest>/.rdirstat/bin/rdirstat-<version>` through a
//!    temp file, `fsync`, mode 0755 and a rename, unless an identical copy is
//!    already there, and asks the installed copy `backup version` again;
//! 4. runs it as `backup serve --dest <dest> --detach` in its own process
//!    group. `--detach` re-executes the binary in a new session (`setsid`),
//!    waits until it answers `/v1/health`, and exits; the service is not a
//!    child of the app and keeps running when the app quits.
//!
//! Everything here blocks (copies, `fsync`, child processes); callers run it
//! on a blocking thread.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rdirstat_jobs::{JOB_FILE, RUNNER_VERSION_ARGS, RunnerVersion, SCHEMA_VERSION, jobs_dir, runner_path};

use super::BackupError;

/// The sidecar's name beside the app's executable (`Contents/MacOS/` in a
/// bundle, `target/<profile>/` in development).
pub(crate) const SIDECAR_NAME: &str = "rdirstat-runner";

/// This app's version; the sidecar and the installed binary must match it.
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long `backup version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `serve --detach` may take; it gives the service 20 s to answer.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);

/// The bundled sidecar.
///
/// # Errors
///
/// [`BackupError::Runner`] when the app's own path is unknown or the sidecar
/// is not beside it.
pub(crate) fn sidecar_path() -> Result<PathBuf, BackupError> {
    let exe = std::env::current_exe().map_err(|error| BackupError::Runner(format!("cannot find the app: {error}")))?;
    let path = exe.with_file_name(SIDECAR_NAME);
    if path.is_file() {
        Ok(path)
    } else {
        Err(BackupError::Runner(format!(
            "the backup runner is missing from the app ({}); build it with `node scripts/build-sidecar.mjs`",
            path.display()
        )))
    }
}

/// Runs `command` to completion within `timeout`, killing it past that.
/// Returns its exit success, stdout and stderr.
fn run_bounded(mut command: Command, timeout: Duration, what: &str) -> Result<(bool, String, String), BackupError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| BackupError::Runner(format!("cannot run {what}: {error}")))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                drop(child.kill());
                drop(child.wait());
                return Err(BackupError::Runner(format!(
                    "{what} did not finish within {} s",
                    timeout.as_secs()
                )));
            }
            Err(error) => return Err(BackupError::Runner(format!("waiting for {what}: {error}"))),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        drop(pipe.read_to_string(&mut stdout));
    }
    if let Some(mut pipe) = child.stderr.take() {
        drop(pipe.read_to_string(&mut stderr));
    }
    Ok((status.success(), stdout, stderr))
}

/// Asks `binary` which version and job schema it is.
///
/// # Errors
///
/// [`BackupError::Runner`] when it does not run, fails, or prints something
/// else (a placeholder sidecar prints its instructions and exits 127).
pub(crate) fn runner_version(binary: &Path) -> Result<RunnerVersion, BackupError> {
    let mut command = Command::new(binary);
    command.args(RUNNER_VERSION_ARGS);
    let what = format!("{} backup version", binary.display());
    let (ok, stdout, stderr) = run_bounded(command, VERSION_TIMEOUT, &what)?;
    if !ok {
        return Err(BackupError::Runner(format!("{what} failed: {}", stderr.trim())));
    }
    serde_json::from_str(stdout.trim())
        .map_err(|error| BackupError::Runner(format!("{what} printed {:?}: {error}", stdout.trim())))
}

/// Refuses a binary that is not this app's version and schema.
fn check_is_ours(version: &RunnerVersion, binary: &Path) -> Result<(), BackupError> {
    if version.version != APP_VERSION || version.schema_version != SCHEMA_VERSION {
        return Err(BackupError::Runner(format!(
            "{} is version {} (job schema {}), not this app's {APP_VERSION} (job schema {SCHEMA_VERSION})",
            binary.display(),
            version.version,
            version.schema_version
        )));
    }
    Ok(())
}

/// The newest `schema_version` of any `job.json` on the drive; 0 with none.
/// A job file that does not parse is skipped: it cannot be run by anyone.
pub(crate) fn newest_job_schema(dest: &Path) -> u32 {
    #[derive(serde::Deserialize)]
    struct Schema {
        schema_version: u32,
    }
    let Ok(entries) = std::fs::read_dir(jobs_dir(dest)) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read(entry.path().join(JOB_FILE)).ok())
        .filter_map(|bytes| serde_json::from_slice::<Schema>(&bytes).ok())
        .map(|schema| schema.schema_version)
        .max()
        .unwrap_or(0)
}

/// Refuses to launch `version` on a drive holding a job written for a newer
/// schema than it speaks.
///
/// # Errors
///
/// [`BackupError::Incompatible`].
pub(crate) fn check_launchable(version: &RunnerVersion, dest: &Path) -> Result<(), BackupError> {
    let newest = newest_job_schema(dest);
    if newest > version.schema_version {
        return Err(BackupError::Incompatible(format!(
            "{} holds a job with schema {newest}; runner {} speaks schema {}. Update the app.",
            dest.display(),
            version.version,
            version.schema_version
        )));
    }
    Ok(())
}

fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {}
        _ => return false,
    }
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Copies `sidecar` to `<dest>/.rdirstat/bin/rdirstat-<version>` atomically
/// (temp file in the same directory, `fsync`, mode 0755, rename, `fsync` of
/// the directory) and returns that path. An identical copy already there is
/// kept; a different one (a half-copied or replaced file) is replaced.
///
/// # Errors
///
/// [`BackupError::Runner`] for any I/O failure, naming the path.
pub(crate) fn install(dest: &Path, sidecar: &Path, version: &str) -> Result<PathBuf, BackupError> {
    let target = runner_path(dest, version);
    if same_contents(sidecar, &target) {
        return Ok(target);
    }
    let io = |what: &str, path: &Path, error: std::io::Error| {
        BackupError::Runner(format!("cannot {what} {}: {error}", path.display()))
    };
    let Some(dir) = target.parent() else {
        return Err(BackupError::Runner(format!("{} has no directory", target.display())));
    };
    std::fs::create_dir_all(dir).map_err(|error| io("create", dir, error))?;
    let temp = dir.join(format!(".rdirstat-{version}.tmp-{}", std::process::id()));
    let copied = (|| {
        let mut from = std::fs::File::open(sidecar).map_err(|error| io("read", sidecar, error))?;
        let mut to = std::fs::File::create(&temp).map_err(|error| io("create", &temp, error))?;
        std::io::copy(&mut from, &mut to).map_err(|error| io("write", &temp, error))?;
        to.set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(|error| io("chmod", &temp, error))?;
        to.sync_all().map_err(|error| io("fsync", &temp, error))?;
        std::fs::rename(&temp, &target).map_err(|error| io("publish", &target, error))
    })();
    if copied.is_err() {
        drop(std::fs::remove_file(&temp));
    }
    copied?;
    // The rename is durable once the directory is; a failure here leaves a
    // complete binary that the next install compares and keeps.
    if let Ok(handle) = std::fs::File::open(dir) {
        drop(handle.sync_all());
    }
    Ok(target)
}

/// Runs `binary backup serve --dest <dest> --detach` and waits for it to
/// report the service up. The service itself runs in a new session and is
/// not this process's child.
///
/// # Errors
///
/// [`BackupError::Runner`] with what the binary printed when it fails (for
/// example, another live service holds the drive's lock).
pub(crate) fn launch(binary: &Path, dest: &Path) -> Result<(), BackupError> {
    let mut command = Command::new(binary);
    command
        .arg("backup")
        .arg("serve")
        .arg("--dest")
        .arg(dest)
        .arg("--detach")
        // Its own process group: a signal to the app's group (a terminal's
        // Ctrl-C in development) does not reach it or the service.
        .process_group(0);
    let what = format!("{} backup serve --detach", binary.display());
    let (ok, stdout, stderr) = run_bounded(command, LAUNCH_TIMEOUT, &what)?;
    if ok {
        tracing::info!(dest = %dest.display(), output = stdout.trim(), "started the backup service");
        Ok(())
    } else {
        Err(BackupError::Runner(format!(
            "the backup service did not start: {}",
            stderr.trim()
        )))
    }
}

/// Steps 1-4 of the module doc: check the sidecar, install it, check the
/// installed copy, launch it.
///
/// # Errors
///
/// The first step that refuses.
pub(crate) fn install_and_launch(dest: &Path, sidecar: &Path) -> Result<PathBuf, BackupError> {
    let version = runner_version(sidecar)?;
    check_is_ours(&version, sidecar)?;
    check_launchable(&version, dest)?;
    let installed = install(dest, sidecar, &version.version)?;
    let installed_version = runner_version(&installed)?;
    check_is_ours(&installed_version, &installed)?;
    launch(&installed, dest)?;
    Ok(installed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "test code: a failed fixture must abort loudly")]

    use std::os::unix::fs::MetadataExt as _;

    use super::*;

    /// A shell script that answers `backup version` like the CLI, and records
    /// any other invocation in `<dir>/calls`.
    fn fake_runner(dir: &Path, version: &str, schema: u32) -> PathBuf {
        let path = dir.join("rdirstat-runner");
        let calls = dir.join("calls");
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$1 $2\" = 'backup version' ]; then\n\
               echo '{{\"version\":\"{version}\",\"schema_version\":{schema}}}'\n\
               exit 0\n\
             fi\n\
             echo \"$@\" >> '{}'\n",
            calls.display()
        );
        std::fs::write(&path, script).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn write_job(dest: &Path, id: &str, schema: u32) {
        let dir = jobs_dir(dest).join(id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(JOB_FILE),
            format!("{{\"schema_version\":{schema},\"id\":\"{id}\"}}"),
        )
        .expect("write");
    }

    #[test]
    fn install_is_atomic_executable_and_idempotent() {
        let tools = tempfile::tempdir().expect("tempdir");
        let dest = tempfile::tempdir().expect("tempdir");
        let sidecar = fake_runner(tools.path(), APP_VERSION, SCHEMA_VERSION);

        let installed = install(dest.path(), &sidecar, APP_VERSION).expect("install");
        assert_eq!(installed, runner_path(dest.path(), APP_VERSION));
        assert_eq!(
            std::fs::read(&installed).expect("read"),
            std::fs::read(&sidecar).expect("read")
        );
        assert_eq!(
            std::fs::metadata(&installed).expect("meta").permissions().mode() & 0o777,
            0o755
        );
        let leftovers: Vec<_> = std::fs::read_dir(installed.parent().expect("dir"))
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no temp file is left: {leftovers:?}");

        // Identical: kept as is (same inode).
        let ino = std::fs::metadata(&installed).expect("meta").ino();
        install(dest.path(), &sidecar, APP_VERSION).expect("reinstall");
        assert_eq!(std::fs::metadata(&installed).expect("meta").ino(), ino);

        // Different (a truncated copy): replaced.
        std::fs::write(&installed, b"#!/bin/sh\n").expect("truncate");
        install(dest.path(), &sidecar, APP_VERSION).expect("repair");
        assert_eq!(
            std::fs::read(&installed).expect("read"),
            std::fs::read(&sidecar).expect("read")
        );
        assert_eq!(runner_version(&installed).expect("version").version, APP_VERSION);
    }

    #[test]
    fn a_runner_older_than_a_job_on_the_drive_is_refused() {
        let dest = tempfile::tempdir().expect("tempdir");
        let version = RunnerVersion {
            version: APP_VERSION.to_owned(),
            schema_version: SCHEMA_VERSION,
        };
        assert_eq!(newest_job_schema(dest.path()), 0);
        check_launchable(&version, dest.path()).expect("an empty drive takes any runner");
        write_job(dest.path(), "old", SCHEMA_VERSION);
        std::fs::create_dir_all(jobs_dir(dest.path()).join("garbage")).expect("mkdir");
        std::fs::write(jobs_dir(dest.path()).join("garbage").join(JOB_FILE), b"{").expect("write");
        check_launchable(&version, dest.path()).expect("same schema");
        write_job(dest.path(), "new", SCHEMA_VERSION + 1);
        assert_eq!(newest_job_schema(dest.path()), SCHEMA_VERSION + 1);
        let refused = check_launchable(&version, dest.path()).expect_err("older runner");
        assert!(matches!(refused, BackupError::Incompatible(_)), "{refused}");
    }

    #[test]
    fn a_placeholder_or_foreign_sidecar_is_never_installed() {
        let tools = tempfile::tempdir().expect("tempdir");
        let dest = tempfile::tempdir().expect("tempdir");

        let placeholder = tools.path().join("placeholder");
        std::fs::write(&placeholder, "#!/bin/sh\necho 'build the sidecar' >&2\nexit 127\n").expect("write");
        std::fs::set_permissions(&placeholder, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let refused = install_and_launch(dest.path(), &placeholder).expect_err("placeholder");
        assert!(refused.to_string().contains("build the sidecar"), "{refused}");

        let foreign = fake_runner(tools.path(), "0.0.0-other", SCHEMA_VERSION);
        let refused = install_and_launch(dest.path(), &foreign).expect_err("foreign version");
        assert!(refused.to_string().contains("not this app's"), "{refused}");
        assert!(
            !dest.path().join(rdirstat_jobs::RDIRSTAT_DIR).exists(),
            "nothing was written to the drive"
        );
    }

    #[test]
    fn launch_runs_serve_detach_for_the_destination() {
        let tools = tempfile::tempdir().expect("tempdir");
        let dest = tempfile::tempdir().expect("tempdir");
        let sidecar = fake_runner(tools.path(), APP_VERSION, SCHEMA_VERSION);
        let installed = install_and_launch(dest.path(), &sidecar).expect("launch");
        assert_eq!(installed, runner_path(dest.path(), APP_VERSION));
        let calls = std::fs::read_to_string(tools.path().join("calls")).expect("calls");
        assert_eq!(
            calls.trim(),
            format!("backup serve --dest {} --detach", dest.path().display())
        );
    }
}
