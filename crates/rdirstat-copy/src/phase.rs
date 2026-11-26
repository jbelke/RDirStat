//! Wall time per backup phase, for the throughput harness
//! (`scripts/bench-backup.sh`, `.settings/docs/12-THROUGHPUT.md` slice 0).
//!
//! Built only with the `phase-timing` feature. Without it [`enter`] returns a
//! zero-sized guard and every function here is an empty inline body, so a
//! production build carries no clock read, no atomic and no thread-local.
//!
//! Times are *self* times: a phase entered inside another (a byte compare
//! inside a tree compare inside an item) is subtracted from the outer one, so
//! the phases add up to the time spent inside any of them and never count a
//! second twice. Each thread keeps its own stack; totals are global.

/// One timed phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Building the plan: the source walk, rules, roll-up choice.
    Plan,
    /// Apply-time re-stats of the source: `unchanged`, roll-up scope walks.
    Restat,
    /// `ditto` processes (one item, or a chunk of files).
    Ditto,
    /// `clonefile(2)` of a landed duplicate.
    Clone,
    /// Byte-for-byte reads of both sides (`compare_files`, `contents_differ`).
    Compare,
    /// The rest of a verify: stats, directory listings, xattr names.
    CompareMeta,
    /// SHA-256 of a file (`sha256_file`).
    Hash,
    /// The job's journal: `events.jsonl` append + `fsync`, `state.json`.
    Journal,
    /// Catalog Parquet: plan rows, hashes, members, receipts, mirror, index.
    Catalog,
    /// Per-item work not in any phase above: path checks, receipt rows,
    /// staging renames, bookkeeping.
    Item,
}

impl Phase {
    /// Every phase, in report order.
    pub const ALL: [Phase; 10] = [
        Phase::Plan,
        Phase::Restat,
        Phase::Ditto,
        Phase::Clone,
        Phase::Compare,
        Phase::CompareMeta,
        Phase::Hash,
        Phase::Journal,
        Phase::Catalog,
        Phase::Item,
    ];

    /// The key used in the harness's JSON.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Phase::Plan => "walk_plan",
            Phase::Restat => "restat",
            Phase::Ditto => "ditto",
            Phase::Clone => "clone",
            Phase::Compare => "compare",
            Phase::CompareMeta => "compare_meta",
            Phase::Hash => "hash",
            Phase::Journal => "journal",
            Phase::Catalog => "catalog",
            Phase::Item => "item_overhead",
        }
    }
}

/// One phase's totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    /// Self time, nanoseconds.
    pub nanos: u64,
    /// Times the phase was entered.
    pub calls: u64,
    /// Payload bytes the phase moved, where it is known.
    pub bytes: u64,
}

/// Whether this build times phases at all.
#[must_use]
#[inline]
pub const fn enabled() -> bool {
    cfg!(feature = "phase-timing")
}

pub use imp::{Guard, add_bytes, add_bytes_with, enter, reset, snapshot};

#[cfg(not(feature = "phase-timing"))]
mod imp {
    use super::{Phase, Totals};

    /// Ends its phase when dropped.
    #[derive(Debug)]
    #[must_use = "the phase ends when the guard is dropped"]
    pub struct Guard;

    /// Starts timing `phase` until the guard drops.
    #[inline]
    pub fn enter(_phase: Phase) -> Guard {
        Guard
    }

    /// Adds payload bytes to `phase`.
    #[inline]
    pub fn add_bytes(_phase: Phase, _bytes: u64) {}

    /// Adds the bytes `count` returns; `count` is never called in a build
    /// without the feature.
    #[inline]
    pub fn add_bytes_with(_phase: Phase, _count: impl FnOnce() -> u64) {}

    /// Zeroes every total.
    #[inline]
    pub fn reset() {}

    /// Every phase's totals, in [`Phase::ALL`] order. Empty without the
    /// feature.
    #[must_use]
    pub fn snapshot() -> Vec<(Phase, Totals)> {
        Vec::new()
    }
}

#[cfg(feature = "phase-timing")]
mod imp {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use super::{Phase, Totals};

    const N: usize = Phase::ALL.len();

    static NANOS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
    static CALLS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
    static BYTES: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

    struct Frame {
        phase: Phase,
        start: Instant,
        /// Time spent in phases entered inside this one.
        children: u64,
    }

    thread_local! {
        static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    }

    const fn slot(phase: Phase) -> usize {
        phase as usize
    }

    /// Ends its phase when dropped.
    #[derive(Debug)]
    #[must_use = "the phase ends when the guard is dropped"]
    pub struct Guard {
        _not_send: std::marker::PhantomData<*const ()>,
    }

    /// Starts timing `phase` until the guard drops.
    pub fn enter(phase: Phase) -> Guard {
        STACK.with(|stack| {
            stack.borrow_mut().push(Frame {
                phase,
                start: Instant::now(),
                children: 0,
            });
        });
        Guard {
            _not_send: std::marker::PhantomData,
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            STACK.with(|stack| {
                let mut stack = stack.borrow_mut();
                let Some(frame) = stack.pop() else { return };
                let total = u64::try_from(frame.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let own = total.saturating_sub(frame.children);
                NANOS[slot(frame.phase)].fetch_add(own, Ordering::Relaxed);
                CALLS[slot(frame.phase)].fetch_add(1, Ordering::Relaxed);
                if let Some(parent) = stack.last_mut() {
                    parent.children = parent.children.saturating_add(total);
                }
            });
        }
    }

    /// Adds payload bytes to `phase`.
    pub fn add_bytes(phase: Phase, bytes: u64) {
        BYTES[slot(phase)].fetch_add(bytes, Ordering::Relaxed);
    }

    /// Adds the bytes `count` returns.
    pub fn add_bytes_with(phase: Phase, count: impl FnOnce() -> u64) {
        add_bytes(phase, count());
    }

    /// Zeroes every total.
    pub fn reset() {
        for index in 0..N {
            NANOS[index].store(0, Ordering::Relaxed);
            CALLS[index].store(0, Ordering::Relaxed);
            BYTES[index].store(0, Ordering::Relaxed);
        }
    }

    /// Every phase's totals, in [`Phase::ALL`] order.
    #[must_use]
    pub fn snapshot() -> Vec<(Phase, Totals)> {
        Phase::ALL
            .iter()
            .map(|&phase| {
                let index = slot(phase);
                (
                    phase,
                    Totals {
                        nanos: NANOS[index].load(Ordering::Relaxed),
                        calls: CALLS[index].load(Ordering::Relaxed),
                        bytes: BYTES[index].load(Ordering::Relaxed),
                    },
                )
            })
            .collect()
    }
}

#[cfg(all(test, feature = "phase-timing"))]
mod tests {
    use super::*;

    #[test]
    fn a_nested_phase_is_not_counted_twice() {
        reset();
        {
            let _outer = enter(Phase::Item);
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _inner = enter(Phase::Hash);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let totals: std::collections::HashMap<_, _> = snapshot().into_iter().map(|(p, t)| (p.key(), t)).collect();
        let item = totals["item_overhead"];
        let hash = totals["hash"];
        assert_eq!((item.calls, hash.calls), (1, 1));
        assert!(hash.nanos >= 20_000_000);
        assert!(item.nanos >= 20_000_000 && item.nanos < 39_000_000, "item {item:?}");
    }
}
