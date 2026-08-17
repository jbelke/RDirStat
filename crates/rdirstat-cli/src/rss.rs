//! Peak resident-set sampling.
//!
//! `getrusage(2)` would give `ru_maxrss` exactly, and the desktop build will
//! use it — through the one audited `unsafe` module in `rdirstat-scan`, not
//! here. This crate is `#![forbid(unsafe_code)]`, so the CLI *samples* instead:
//! a background thread reads `ps -o rss=` for its own pid and keeps the maximum.
//!
//! That is an honest measurement with two documented limits: it is a sample, so
//! a spike between polls is missed, and it resolves to kibibytes. It is
//! reported with its source so a number from this path is never mistaken for
//! `ru_maxrss`. The exact, non-sampled part of the memory story — arena bytes,
//! name-blob bytes, directory-index bytes — is counted, not sampled.

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How the peak RSS number was obtained, so a reader can judge it.
pub(crate) const RSS_SOURCE: &str = "ps-rss-sampling@50ms";

/// A running sampler. Dropping it without [`Sampler::finish`] still stops the
/// thread, but discards the reading.
#[derive(Debug)]
pub(crate) struct Sampler {
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Sampler {
    /// Starts sampling this process's resident set every 50 ms.
    pub(crate) fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_peak = Arc::clone(&peak);
        let thread_stop = Arc::clone(&stop);
        let pid = std::process::id();
        let handle = thread::Builder::new()
            .name("rss-sampler".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    if let Some(bytes) = sample(pid) {
                        thread_peak.fetch_max(bytes, Ordering::Relaxed);
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                if let Some(bytes) = sample(pid) {
                    thread_peak.fetch_max(bytes, Ordering::Relaxed);
                }
            })
            .ok();
        Self { peak, stop, handle }
    }

    /// Stops sampling and returns the largest resident set observed, in bytes.
    ///
    /// `None` means no sample succeeded, which is a real answer: reporting a
    /// zero would look like a measurement.
    pub(crate) fn finish(mut self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
        match self.peak.load(Ordering::Relaxed) {
            0 => None,
            bytes => Some(bytes),
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

/// One `ps` reading, in bytes. `ps -o rss=` prints kibibytes.
fn sample(pid: u32) -> Option<u64> {
    let output = Command::new("/bin/ps")
        .args(["-o", "rss=", "-p"])
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let kib: u64 = text.trim().parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_this_process_returns_a_plausible_resident_set() {
        let Some(bytes) = sample(std::process::id()) else {
            // A machine without /bin/ps is a legitimate environment; the
            // sampler reports None there rather than a fabricated zero.
            return;
        };
        assert!(bytes > 512 * 1024, "a live process holds more than 512 KiB: {bytes}");
    }

    #[test]
    fn a_started_sampler_produces_a_reading_or_an_honest_none() {
        let sampler = Sampler::start();
        thread::sleep(Duration::from_millis(60));
        let peak = sampler.finish();
        if let Some(bytes) = peak {
            assert!(bytes > 0);
        }
    }
}
