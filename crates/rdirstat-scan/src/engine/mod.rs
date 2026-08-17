//! The two schedulers.
//!
//! Both drive the **same** [`DirReader`](crate::DirReader) through the same
//! one-directory contract and hand every batch to the same
//! [`ScanBuilder`](crate::builder::ScanBuilder). They differ only in when a
//! directory is read. That is what makes the differential test meaningful: if
//! the two ever disagree, the disagreement is in the scheduler, because nothing
//! else is different.

pub(crate) mod parallel;
pub(crate) mod sequential;

use core::num::NonZeroU16;

use rdirstat_core::ScanError;

use crate::cancel::CancelToken;
use crate::progress::{ArenaFootprint, Counters, CurrentDir};
use crate::reader::DirReader;

/// Which scheduler runs the traversal.
///
/// The worker count is explicit and is recorded in the result's
/// [`ScanOptions`](rdirstat_core::ScanOptions), because a scan whose throughput
/// you cannot attribute to a setting is not a measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Engine {
    /// One thread, one explicit stack. The reference implementation: no
    /// channels, no threads, nothing to get wrong.
    Sequential,
    /// A fixed pool of readers behind bounded channels and an exact
    /// pending-directory counter.
    Parallel {
        /// How many reader threads.
        workers: NonZeroU16,
    },
}

impl Engine {
    /// The default worker count.
    ///
    /// **Not `num_cpus`.** docs/02-SCANNER.md is explicit that the default is
    /// chosen from measured throughput and memory, and that too much
    /// concurrency turns a sequential disk workload into seek contention. No
    /// such measurement exists yet, so this is a deliberately conservative
    /// four, recorded in every result so a benchmark can move it on evidence.
    pub const DEFAULT_WORKERS: u16 = 4;

    /// A parallel engine with `workers` readers, clamped to at least one.
    #[must_use]
    pub const fn parallel(workers: u16) -> Self {
        Self::Parallel {
            workers: match NonZeroU16::new(workers) {
                Some(workers) => workers,
                None => NonZeroU16::MIN,
            },
        }
    }

    /// The parallel engine at [`Engine::DEFAULT_WORKERS`].
    #[must_use]
    pub const fn parallel_default() -> Self {
        Self::parallel(Self::DEFAULT_WORKERS)
    }

    /// The effective worker count, which is what the result records.
    #[must_use]
    pub const fn workers(self) -> u16 {
        match self {
            Self::Sequential => 1,
            Self::Parallel { workers } => workers.get(),
        }
    }

    /// A short, stable label for benchmark output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel { .. } => "parallel",
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::parallel_default()
    }
}

/// How a traversal ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Completion {
    /// Every reachable directory was visited or explicitly skipped.
    Finished,
    /// Cancellation was observed. Partial state is discarded by the caller.
    Cancelled,
}

/// Everything a scheduler shares with its readers.
pub(crate) struct EngineContext<'a> {
    pub(crate) reader: &'a dyn DirReader,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) counters: &'a Counters,
    pub(crate) current: &'a CurrentDir,
    pub(crate) memory_limit: Option<u64>,
}

impl core::fmt::Debug for EngineContext<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EngineContext")
            .field("reader", &self.reader.name())
            .field("memory_limit", &self.memory_limit)
            .finish_non_exhaustive()
    }
}

/// Ends the scan when the projected arena footprint crosses the configured
/// ceiling, rather than at allocation failure.
///
/// # Errors
///
/// [`ScanError::MemoryLimit`], which is fatal.
pub(crate) fn check_memory(footprint: ArenaFootprint, limit: Option<u64>) -> Result<(), ScanError> {
    if let Some(limit) = limit
        && footprint.projected_peak_bytes > limit
    {
        return Err(ScanError::MemoryLimit {
            projected_bytes: footprint.projected_peak_bytes,
            limit_bytes: limit,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_counts_are_explicit_and_never_zero() {
        assert_eq!(Engine::Sequential.workers(), 1);
        assert_eq!(Engine::parallel(0).workers(), 1, "zero workers would never terminate");
        assert_eq!(Engine::parallel(9).workers(), 9);
        assert_eq!(Engine::default().workers(), Engine::DEFAULT_WORKERS);
        assert_eq!(Engine::Sequential.label(), "sequential");
    }

    #[test]
    fn the_memory_ceiling_is_checked_against_the_projection() {
        let footprint = ArenaFootprint {
            resident_bytes: 100,
            projected_peak_bytes: 500,
            ..ArenaFootprint::default()
        };
        assert!(check_memory(footprint, None).is_ok());
        assert!(
            check_memory(footprint, Some(500)).is_ok(),
            "the limit itself is allowed"
        );
        let error = check_memory(footprint, Some(499)).expect_err("over the ceiling");
        assert!(matches!(error, ScanError::MemoryLimit { .. }));
        assert!(error.is_fatal());
    }
}
