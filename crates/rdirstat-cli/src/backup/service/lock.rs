//! `<dest>/.rdirstat/run/lock`: one service per drive.
//!
//! The lock is created by writing a temp file and `link(2)`ing it to `lock`,
//! which fails if `lock` exists, so it never exists half-written and two
//! services never both win. A lock is taken over only when it is provably
//! stale: same host, and another boot or a pid that is not running. A lock
//! from another host, or one that does not parse, is refused with its path so
//! a person can judge it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rdirstat_jobs::LockInfo;

use super::sys;

/// Why the lock could not be taken.
#[derive(Debug)]
pub(crate) enum LockRefusal {
    /// A live service holds it.
    Held(LockInfo),
    /// It names another host, or does not parse.
    Unjudgeable(String),
}

impl std::fmt::Display for LockRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held(info) => write!(
                f,
                "a backup service already runs for this drive (pid {} on {}, version {})",
                info.pid, info.host, info.version
            ),
            Self::Unjudgeable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for LockRefusal {}

/// This process's identity as a lock holder.
pub(crate) fn me(now_ms: i64) -> LockInfo {
    LockInfo {
        pid: std::process::id(),
        host: sys::hostname(),
        boot_id: sys::boot_id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at_ms: now_ms,
    }
}

/// Whether `info` names a process that is running now, as far as this host
/// can tell. Another host's lock counts as alive: it cannot be disproved.
pub(crate) fn alive(info: &LockInfo) -> bool {
    if info.host != sys::hostname() {
        return true;
    }
    info.boot_id == sys::boot_id() && sys::pid_alive(info.pid)
}

/// A held lock; dropping it removes the file if it is still ours.
#[derive(Debug)]
pub(crate) struct Lock {
    path: PathBuf,
    info: LockInfo,
    /// The stale lock this one replaced, if any.
    pub(crate) took_over: Option<LockInfo>,
}

impl Lock {
    /// Takes the lock at `path` for `info`, taking over a stale one.
    ///
    /// # Errors
    ///
    /// A [`LockRefusal`] (downcastable) when it is held or unjudgeable;
    /// otherwise an I/O failure.
    pub(crate) fn acquire(path: &Path, info: LockInfo) -> Result<Self> {
        let mut took_over = None;
        for _ in 0..3 {
            if publish(path, &info)? {
                return Ok(Self {
                    path: path.to_path_buf(),
                    info,
                    took_over,
                });
            }
            let existing = match read(path) {
                Ok(Some(existing)) => existing,
                Ok(None) => continue,
                Err(why) => return Err(LockRefusal::Unjudgeable(why).into()),
            };
            if existing.host != info.host {
                return Err(LockRefusal::Unjudgeable(format!(
                    "{} is held by pid {} on another host ({}); if that service is gone, delete the file",
                    path.display(),
                    existing.pid,
                    existing.host
                ))
                .into());
            }
            if alive(&existing) {
                return Err(LockRefusal::Held(existing).into());
            }
            // Move the stale lock aside rather than delete it: the rename is
            // atomic, so of two services racing to take over, the loser
            // finds the winner's fresh lock on its next `publish`.
            let aside = path.with_file_name(format!("lock.stale-{}", existing.pid));
            match std::fs::rename(path, &aside) {
                Ok(()) => {
                    let _ignored = std::fs::remove_file(&aside);
                    took_over = Some(existing);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("cannot move {} aside", path.display())),
            }
        }
        bail!("could not take {} after three attempts", path.display())
    }

    /// The holder this lock records.
    pub(crate) fn info(&self) -> &LockInfo {
        &self.info
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if read(&self.path).ok().flatten().as_ref() == Some(&self.info) {
            let _ignored = std::fs::remove_file(&self.path);
        }
    }
}

/// Reads a lock file: `Ok(None)` when absent, `Err` with a message when it
/// exists but cannot be read or parsed.
pub(crate) fn read(path: &Path) -> std::result::Result<Option<LockInfo>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            format!(
                "{} is not a lock file ({error}); if no service runs, delete it",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// Writes `info` to a temp file and links it to `path`. `Ok(false)` when
/// `path` already exists.
fn publish(path: &Path, info: &LockInfo) -> Result<bool> {
    let tmp = path.with_file_name(format!(".lock.tmp-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(info)?)?;
        file.sync_all()?;
    }
    let linked = std::fs::hard_link(&tmp, path);
    let _ignored = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot create {}", path.display())),
    }
}
