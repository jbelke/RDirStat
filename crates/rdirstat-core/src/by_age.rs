//! Ages: "how old is the data sitting here", answered without sorting a date
//! column and reading down it.
//!
//! ## Why the edges are whole days and not calendar months
//!
//! The thresholds are 7, 30, 90, 365 and 730 **days**, each of them exactly
//! 86,400 seconds. Calendar arithmetic was the obvious alternative and it was
//! rejected twice over.
//!
//! First, "one month ago" is not a duration. It is an operation on a civil
//! date, a civil date needs a timezone, and a timezone needs a tzdata lookup —
//! I/O, in the one crate whose premise is that it performs none. Second, the
//! answer would move under the user's feet: the same file is "within the last
//! month" on March 30th and outside it on March 31st, for a reason that is
//! invisible from the screen.
//!
//! A fixed 86,400-second day has neither problem. Unix time ignores leap
//! seconds by construction, so a day is *always* exactly 86,400 of them, and
//! `now - mtime` is integer subtraction with no calendar anywhere in it. The
//! price is that a boundary drifts by up to an hour against local wall-clock
//! time across a daylight-saving transition. For a question whose entire shape
//! is "roughly how long since anything touched this", an hour of slop at one
//! edge is not worth a timezone database.
//!
//! The particular edges are the intervals people already reason in — this
//! week, this month, this quarter, this year, last year, and everything before
//! that — rather than a log scale chosen for its arithmetic. The oldest bucket
//! is open-ended on purpose: "untouched for over two years" is the sentence
//! this report exists to produce, and splitting it into 5-year and 10-year
//! rows would spread the one actionable number across rows nobody treats
//! differently.
//!
//! ## `now` is a parameter, not a call
//!
//! Nothing here calls `SystemTime::now()`. A function whose output depends on
//! the wall clock cannot be asserted on — the same input produces a different
//! bucket tomorrow — so every entry point takes `now_unix_seconds: i64` and
//! the caller supplies it. That also keeps this crate I/O-free, and it lets the
//! shell pin one `now` per query so a row count and the leaderboard underneath
//! it are computed against the same instant instead of two instants a few
//! milliseconds apart.
//!
//! **A caller must pass the same `now` to [`age_buckets`] and to
//! [`age_bucket_entries`].** Different values are not an error and will not be
//! detected; they just produce a list that disagrees with the count above it.
//!
//! ## `mtime` is hostile input
//!
//! The scanner stores `st_mtime` exactly as the filesystem reported it —
//! signed, whole seconds, never clamped — because a pre-1970 timestamp is a
//! real thing on a real disk (restored archives, tarballs with epoch-zero
//! stamps, files copied off media with a dead RTC) and clamping it would
//! destroy evidence. Two consequences fall out here:
//!
//! - **`now - mtime` can overflow `i64`.** `now = i64::MAX` against
//!   `mtime = -1` is not representable. Every age is therefore computed with
//!   [`i64::saturating_sub`], which cannot panic and whose saturated ends land
//!   in exactly the right buckets: an unrepresentably old file is oldest, an
//!   unrepresentably future-dated one is newest.
//! - **A file can be dated in the future**, which makes its age negative. That
//!   happens for clock skew, for build systems that stamp artefacts forward,
//!   and for anything `touch -t 2099…` has been run over. Such a file goes in
//!   the **newest** bucket. It is the only defensible place: the bucket's real
//!   meaning is "as recent as anything gets", a future-dated file is by
//!   definition not stale, and the alternative — an unsigned subtraction
//!   wrapping to a colossal age — would file tomorrow's file under "older than
//!   two years" and invite the user to delete it.
//!
//! ## What gets bucketed
//!
//! Files, by their own `mtime`. Directories are deliberately **not** bucketed.
//! A directory's size is its subtree total, so bucketing both a directory and
//! the files inside it would count the same bytes twice and the rows would stop
//! summing to the subtree. A bucket is a partition of the leaves, and it says
//! so. A directory's own `mtime` is also a poor proxy for its contents' age —
//! it changes when an entry is added or removed, not when one is edited.
//!
//! Both byte totals are reported per bucket. Membership does not depend on
//! either: it is decided by time alone.

use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::tree::Tree;

/// Seconds in a day.
///
/// Exact, not approximate: Unix time skips leap seconds, so this is the true
/// length of a day in the units `mtime` is measured in.
pub const DAY_SECONDS: i64 = 86_400;

/// Bucket edges in **seconds of age**, ascending, each the **inclusive lower
/// bound** of the bucket above it.
///
/// Whole days on purpose; see the module docs. These are the intervals a person
/// already reasons in — this week, this month, this quarter, this year, last
/// year — not a log scale.
pub const AGE_BUCKET_EDGES: [i64; 5] = [
    7 * DAY_SECONDS,   // a week            604,800
    30 * DAY_SECONDS,  // a month-ish     2,592,000
    90 * DAY_SECONDS,  // a quarter-ish   7,776,000
    365 * DAY_SECONDS, // a year         31,536,000
    730 * DAY_SECONDS, // two years      63,072,000
];

/// One more bucket than there are edges: everything newer than the first edge
/// is its own bucket, and everything at or beyond the last edge is another.
pub const AGE_BUCKET_COUNT: usize = AGE_BUCKET_EDGES.len() + 1;

/// How old a file with this `mtime` is, in seconds, relative to
/// `now_unix_seconds`.
///
/// Negative for a file dated in the future. Saturating rather than wrapping or
/// panicking, because `mtime` is whatever the filesystem said and the two
/// extremes of `i64` are both reachable inputs; see the module docs.
#[must_use]
pub const fn age_seconds(now_unix_seconds: i64, mtime: i64) -> i64 {
    now_unix_seconds.saturating_sub(mtime)
}

/// Which bucket an age of `age` seconds falls in, `0` being the newest.
///
/// Buckets are half-open `[lower, upper)` in age, so a file exactly seven days
/// old is in the "7 days" bucket and not the one below it.
///
/// A negative age — a file dated in the future — lands in bucket `0`, because
/// no edge is at or below it. That is the documented policy, not an accident of
/// the search; see the module docs.
#[must_use]
pub fn age_bucket_of(age: i64) -> usize {
    AGE_BUCKET_EDGES.partition_point(|edge| *edge <= age)
}

/// Which bucket a file with this `mtime` falls in, relative to
/// `now_unix_seconds`. The composition of [`age_seconds`] and
/// [`age_bucket_of`], named because it is the operation callers actually want.
#[must_use]
pub fn age_bucket_of_mtime(now_unix_seconds: i64, mtime: i64) -> usize {
    age_bucket_of(age_seconds(now_unix_seconds, mtime))
}

/// The inclusive lower age bound of `bucket` in seconds, or `0` for the newest.
///
/// `0` for the newest bucket is the *nominal* edge. That bucket also absorbs
/// negative ages, so its true lower bound is unbounded below; reporting
/// `i64::MIN` here would only put an absurd number on screen for a case the
/// label "in the last 7 days" already covers.
#[must_use]
pub fn age_bucket_lower(bucket: usize) -> i64 {
    match bucket.checked_sub(1) {
        None => 0,
        Some(index) => AGE_BUCKET_EDGES.get(index).copied().unwrap_or(i64::MAX),
    }
}

/// The exclusive upper age bound of `bucket` in seconds, or `None` for the
/// oldest.
#[must_use]
pub fn age_bucket_upper(bucket: usize) -> Option<i64> {
    AGE_BUCKET_EDGES.get(bucket).copied()
}

/// One row of the age histogram.
///
/// Carries the edges in seconds rather than a rendered label, for the same
/// reason [`SizeBandRow`](crate::SizeBandRow) does: the front end formats them
/// with its own conventions instead of the backend shipping display text that
/// cannot then be re-styled or localised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct AgeBucketRow {
    /// Bucket index, `0` newest. Stable for a given [`AGE_BUCKET_EDGES`].
    pub bucket: u8,
    /// Inclusive lower age bound in seconds; `0` for the newest bucket.
    pub lower_seconds: i64,
    /// Exclusive upper age bound in seconds; `None` for the oldest bucket.
    pub upper_seconds: Option<i64>,
    /// Files in this bucket. Counted even when they contribute no bytes, so a
    /// hard-link repeat is still visible as a name that exists.
    pub files: u64,
    /// Logical bytes of those files, after hard-link policy.
    pub logical: u64,
    /// Allocated bytes of those files, after hard-link policy.
    pub allocated: u64,
}

/// Buckets every file in the subtree at `root` by modification time.
///
/// Always returns exactly [`AGE_BUCKET_COUNT`] rows, including empty ones: a
/// bucket that disappears when it has no members makes the table jump around as
/// the user drills, and "nothing here has been untouched for two years" is an
/// answer worth showing rather than an absence to be inferred.
///
/// `now_unix_seconds` is supplied by the caller; see the module docs for why
/// this crate refuses to read the clock itself.
///
/// Returns `None` if `root` is not a node in this tree.
///
/// Iterative, never recursive — the same reason every other walk in this crate
/// is: a 4096-deep chain is a real input and a stack overflow is not a
/// diagnosable failure.
#[must_use]
pub fn age_buckets(tree: &Tree, root: NodeId, now_unix_seconds: i64) -> Option<Vec<AgeBucketRow>> {
    // A virtual `<Files>` group has no arena node of its own; bucket its owner.
    let start = root.group_owner().unwrap_or(root);
    tree.node(start)?;

    let mut rows: Vec<AgeBucketRow> = (0..AGE_BUCKET_COUNT)
        .map(|bucket| AgeBucketRow {
            bucket: u8::try_from(bucket).unwrap_or(u8::MAX),
            lower_seconds: age_bucket_lower(bucket),
            upper_seconds: age_bucket_upper(bucket),
            files: 0,
            logical: 0,
            allocated: 0,
        })
        .collect();

    let mut stack = vec![start];
    // The arena is finite and acyclic by `Tree`'s own freeze-time validation, so
    // this bound is a backstop against a tree that somehow escaped it rather
    // than an expected limit.
    let mut budget = tree.len().saturating_mul(2).saturating_add(16);

    while let Some(id) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;

        let Some(node) = tree.node(id) else { continue };

        if node.kind().is_file() {
            let bucket = age_bucket_of_mtime(now_unix_seconds, node.mtime);
            if let Some(row) = rows.get_mut(bucket) {
                row.files = row.files.saturating_add(1);
                row.allocated = row.allocated.saturating_add(node.contributed_alloc());
                row.logical = row.logical.saturating_add(node.contributed_size());
            }
        }

        stack.extend(tree.children(id));
    }

    Some(rows)
}

/// One file inside an age bucket, for the breakdown.
///
/// Carries its resolved path because the whole point of expanding a bucket is
/// to find out *which* files are in it; a list of node ids would make the
/// caller issue one `path_of` per row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct AgeBucketEntry {
    /// The arena node, so a row can be selected or revealed.
    pub node: NodeId,
    /// Full path, escaped for display.
    pub path: crate::wire::DisplayPath,
    /// Allocated bytes, after hard-link policy. The quantity this list is
    /// ordered by.
    pub allocated: u64,
    /// Logical bytes, after hard-link policy.
    pub logical: u64,
    /// Modification time in whole Unix seconds. The quantity that placed the
    /// file in this bucket, shipped raw so the front end can render both the
    /// date and the age from one number.
    pub mtime: i64,
    /// Content category index.
    pub category: u8,
}

/// The largest files in one age bucket, biggest first.
///
/// Ordered by **allocated bytes, not by date**, and that is the point of the
/// report. Every file in a bucket is already known to be about as old as every
/// other; the question left over is "of the things nobody has touched in two
/// years, which ones are worth reclaiming", and that is a size question.
///
/// Bounded by `limit` on purpose. A bucket can hold ten million files — the
/// oldest one on a boot volume does — so this is deliberately a *leaderboard*,
/// not an enumeration. The caller is told the true total by [`age_buckets`] and
/// shown the head of the list.
///
/// `now_unix_seconds` **must be the same value passed to [`age_buckets`]**, or
/// the list and the count above it describe two different instants.
///
/// Returns `None` if `root` is not a node in this tree.
#[must_use]
pub fn age_bucket_entries(
    tree: &Tree,
    root: NodeId,
    now_unix_seconds: i64,
    bucket: usize,
    limit: usize,
) -> Option<Vec<AgeBucketEntry>> {
    let start = root.group_owner().unwrap_or(root);
    tree.node(start)?;
    if limit == 0 || bucket >= AGE_BUCKET_COUNT {
        return Some(Vec::new());
    }

    // Collect (allocated, node) for matching files, keeping only the heaviest
    // `limit`. A full sort of ten million entries to show two hundred would be
    // the expensive way to answer the same question.
    let mut best: Vec<(u64, NodeId)> = Vec::with_capacity(limit.min(1024));
    let mut floor = 0_u64;

    let mut stack = vec![start];
    let mut budget = tree.len().saturating_mul(2).saturating_add(16);
    while let Some(id) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(node) = tree.node(id) else { continue };

        if node.kind().is_file() {
            let allocated = node.contributed_alloc();
            if age_bucket_of_mtime(now_unix_seconds, node.mtime) == bucket && (best.len() < limit || allocated > floor)
            {
                best.push((allocated, id));
                // Sorting on every insertion past the cap keeps the vector
                // bounded without a heap; `limit` is small (hundreds), so this
                // is cheaper than it looks and never allocates unboundedly.
                if best.len() > limit {
                    best.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    best.truncate(limit);
                    floor = best.last().map_or(0, |entry| entry.0);
                }
            }
        }

        stack.extend(tree.children(id));
    }

    best.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    best.truncate(limit);

    let mut out = Vec::with_capacity(best.len());
    let mut scratch = Vec::new();
    for (allocated, id) in best {
        let Some(node) = tree.node(id) else { continue };
        scratch.clear();
        // A path that cannot be reconstructed is skipped rather than shown
        // blank: a row naming no file is worse than a shorter list.
        if tree.path_bytes(id, &mut scratch).is_err() {
            continue;
        }
        out.push(AgeBucketEntry {
            node: id,
            path: crate::wire::DisplayPath::from_bytes(&scratch),
            allocated,
            logical: node.contributed_size(),
            mtime: node.mtime,
            category: node.category,
        });
    }
    Some(out)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;
    use crate::dirs::DirTotals;
    use crate::node::{Kind, Node, flags};
    use crate::tree::TreeBuilder;

    /// A fixed instant so every expectation below is a constant. 2023-11-14.
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn the_edges_are_whole_days_because_a_month_is_not_a_duration() {
        // If these ever become calendar months, bucketing needs a timezone and
        // this crate stops being I/O-free.
        assert_eq!(DAY_SECONDS, 86_400);
        assert_eq!(AGE_BUCKET_EDGES[0], 604_800);
        assert_eq!(AGE_BUCKET_EDGES[1], 2_592_000);
        assert_eq!(AGE_BUCKET_EDGES[2], 7_776_000);
        assert_eq!(AGE_BUCKET_EDGES[3], 31_536_000);
        assert_eq!(AGE_BUCKET_EDGES[4], 63_072_000);
        assert_eq!(AGE_BUCKET_COUNT, 6);
    }

    #[test]
    fn buckets_are_half_open_upwards_in_age() {
        assert_eq!(age_bucket_of(0), 0, "a file written this second is newest");
        assert_eq!(age_bucket_of(AGE_BUCKET_EDGES[0] - 1), 0);
        // Exactly on an edge belongs to the older bucket, so "7 days" is a
        // closed lower bound and every second belongs to exactly one bucket.
        assert_eq!(age_bucket_of(AGE_BUCKET_EDGES[0]), 1);
        assert_eq!(age_bucket_of(AGE_BUCKET_EDGES[4]), 5);
        assert_eq!(age_bucket_of(i64::MAX), AGE_BUCKET_COUNT - 1);
    }

    #[test]
    fn every_bucket_has_an_edge_pair_and_only_the_last_is_open() {
        assert_eq!(age_bucket_lower(0), 0);
        for bucket in 0..AGE_BUCKET_COUNT - 1 {
            let upper = age_bucket_upper(bucket).expect("a bounded bucket");
            assert_eq!(upper, age_bucket_lower(bucket + 1), "bucket {bucket} leaves a gap");
        }
        assert_eq!(age_bucket_upper(AGE_BUCKET_COUNT - 1), None);
    }

    #[test]
    fn a_file_dated_in_the_future_is_newest_rather_than_oldest() {
        // Clock skew and `touch -t 2099` are real. An unsigned subtraction would
        // wrap this to a colossal age and file tomorrow's build artefact under
        // "older than two years".
        assert_eq!(age_seconds(NOW, NOW + 5 * DAY_SECONDS), -5 * DAY_SECONDS);
        assert_eq!(age_bucket_of_mtime(NOW, NOW + 5 * DAY_SECONDS), 0);
        assert_eq!(age_bucket_of(-1), 0);

        // The extreme case: an mtime so far forward that `now - mtime` is not
        // representable. Saturating keeps it in the newest bucket instead of
        // panicking in debug and wrapping in release.
        assert_eq!(age_seconds(-1, i64::MAX), i64::MIN);
        assert_eq!(age_bucket_of_mtime(-1, i64::MAX), 0);
        assert_eq!(age_bucket_of_mtime(i64::MIN, i64::MAX), 0);
    }

    #[test]
    fn a_prehistoric_mtime_is_oldest_and_does_not_overflow() {
        // Pre-1970 timestamps survive the scanner unclamped, so they arrive
        // here as negative seconds.
        let nineteen_sixty = -315_619_200_i64;
        assert_eq!(age_bucket_of_mtime(NOW, nineteen_sixty), AGE_BUCKET_COUNT - 1);

        // `now - mtime` is unrepresentable at the extremes; saturation lands on
        // the correct end rather than overflowing.
        assert_eq!(age_seconds(i64::MAX, i64::MIN), i64::MAX);
        assert_eq!(age_bucket_of_mtime(i64::MAX, i64::MIN), AGE_BUCKET_COUNT - 1);
        assert_eq!(age_bucket_of_mtime(NOW, i64::MIN), AGE_BUCKET_COUNT - 1);
    }

    fn push_file(builder: &mut TreeBuilder, parent: NodeId, name: &[u8], bytes: u64, mtime: i64, repeat: bool) {
        let reference = builder.intern(name).expect("interns");
        let mut node = Node::leaf(reference, Kind::File, bytes, bytes, mtime);
        if repeat {
            node = node.with_flags(flags::HARD_LINK_REPEAT);
        }
        builder.push_child(parent, node).expect("links");
    }

    /// ```text
    /// root/                mtime NOW-500d   (a directory: never bucketed)
    ///   fresh.txt          mtime NOW-1d        1 KiB   -> bucket 0
    ///   week.bin           mtime NOW-10d       2 MiB   -> bucket 1
    ///   sub/               mtime NOW-500d   (a directory: never bucketed)
    ///     quarter.bin      mtime NOW-100d      4 MiB   -> bucket 3
    ///     ancient.bin      mtime NOW-1000d     8 MiB   -> bucket 5
    ///     future.bin       mtime NOW+5d       16 MiB   -> bucket 0
    ///     clone.bin        mtime NOW-1000d     8 MiB, hard-link repeat
    ///                                                  -> bucket 5, 0 bytes
    /// ```
    ///
    /// Both directories are dated 500 days back, which is bucket 4. Bucket 4
    /// must stay empty; that is what makes this fixture detect a walk that
    /// starts counting directories.
    fn fixture() -> (Tree, NodeId) {
        let mut builder = TreeBuilder::new();
        let root_name = builder.intern(b"root").expect("interns");
        let root = builder
            .push_node(Node::directory(root_name, NOW - 500 * DAY_SECONDS))
            .expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        push_file(&mut builder, root, b"fresh.txt", 1 << 10, NOW - DAY_SECONDS, false);
        push_file(&mut builder, root, b"week.bin", 2 << 20, NOW - 10 * DAY_SECONDS, false);

        let sub_name = builder.intern(b"sub").expect("interns");
        let sub = builder
            .push_child(root, Node::directory(sub_name, NOW - 500 * DAY_SECONDS))
            .expect("links");
        builder.register_directory(sub, DirTotals::EMPTY).expect("registers");

        push_file(
            &mut builder,
            sub,
            b"quarter.bin",
            4 << 20,
            NOW - 100 * DAY_SECONDS,
            false,
        );
        push_file(
            &mut builder,
            sub,
            b"ancient.bin",
            8 << 20,
            NOW - 1000 * DAY_SECONDS,
            false,
        );
        push_file(&mut builder, sub, b"future.bin", 16 << 20, NOW + 5 * DAY_SECONDS, false);
        push_file(&mut builder, sub, b"clone.bin", 8 << 20, NOW - 1000 * DAY_SECONDS, true);

        (builder.finish().expect("valid"), root)
    }

    #[test]
    fn files_land_in_the_bucket_their_mtime_names() {
        let (tree, root) = fixture();
        let rows = age_buckets(&tree, root, NOW).expect("a root");

        assert_eq!(rows.len(), AGE_BUCKET_COUNT);
        assert_eq!(rows[0].files, 2, "one day old plus one dated in the future");
        assert_eq!(rows[1].files, 1, "ten days old belongs to the 7-day bucket");
        assert_eq!(rows[2].files, 0, "nothing here is 30-90 days old");
        assert_eq!(rows[3].files, 1, "a hundred days old belongs to the 90-day bucket");
        assert_eq!(rows[5].files, 2, "a thousand days old, plus its hard-link repeat");
    }

    #[test]
    fn directories_are_never_bucketed_even_when_their_own_mtime_would_qualify() {
        let (tree, root) = fixture();
        let rows = age_buckets(&tree, root, NOW).expect("a root");
        // Both directories are 500 days old. If either were counted this would
        // be non-zero, and the totals would double-count their subtrees.
        assert_eq!(rows[4].files, 0, "a directory's own mtime must not create a row");
        assert_eq!(rows[4].allocated, 0);
    }

    #[test]
    fn a_hard_link_repeat_is_listed_but_contributes_no_bytes() {
        let (tree, root) = fixture();
        let rows = age_buckets(&tree, root, NOW).expect("a root");

        // clone.bin is a second name for content already counted elsewhere, so
        // it is visible as a file in its bucket while adding nothing to the
        // bucket's bytes.
        assert_eq!(rows[5].files, 2);
        assert_eq!(rows[5].allocated, 8 << 20, "8 MiB once, not twice");
        assert_eq!(rows[5].logical, 8 << 20);
    }

    #[test]
    fn bucket_totals_sum_to_the_subtree_exactly_once() {
        let (tree, root) = fixture();
        let rows = age_buckets(&tree, root, NOW).expect("a root");

        let bucketed: u64 = rows.iter().map(|row| row.allocated).sum();
        let expected = (1_u64 << 10) + (2 << 20) + (4 << 20) + (8 << 20) + (16 << 20);
        assert_eq!(bucketed, expected, "buckets must partition the leaves exactly once");

        let logical: u64 = rows.iter().map(|row| row.logical).sum();
        assert_eq!(logical, expected);

        let files: u64 = rows.iter().map(|row| row.files).sum();
        assert_eq!(files, 6, "every file once, no directories");
    }

    #[test]
    fn every_bucket_is_reported_even_when_empty() {
        let (tree, root) = fixture();
        let rows = age_buckets(&tree, root, NOW).expect("a root");
        // A bucket that vanishes when empty makes the table jump as the user
        // drills, and "nothing here is older than two years" is an answer.
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(usize::from(row.bucket), index);
            assert_eq!(row.lower_seconds, age_bucket_lower(index));
            assert_eq!(row.upper_seconds, age_bucket_upper(index));
        }
    }

    #[test]
    fn an_unknown_node_has_no_buckets() {
        let (tree, _) = fixture();
        assert!(age_buckets(&tree, NodeId::from_raw(9_999), NOW).is_none());
        assert!(age_bucket_entries(&tree, NodeId::from_raw(9_999), NOW, 0, 10).is_none());
    }

    #[test]
    fn the_leaderboard_is_ordered_by_allocated_bytes_not_by_date() {
        let (tree, root) = fixture();
        let entries = age_bucket_entries(&tree, root, NOW, 0, 10).expect("a root");

        // future.bin is 16 MiB and fresh.txt is 1 KiB. Ordering by date would
        // put the newer of the two first; ordering by size puts the one worth
        // reclaiming first, which is the question the report answers.
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, vec!["root/sub/future.bin", "root/fresh.txt"]);
        assert_eq!(entries[0].allocated, 16 << 20);
        assert_eq!(entries[0].mtime, NOW + 5 * DAY_SECONDS);
    }

    #[test]
    fn the_leaderboard_is_bounded_by_its_limit() {
        let (tree, root) = fixture();
        let entries = age_bucket_entries(&tree, root, NOW, 5, 1).expect("a root");
        assert_eq!(entries.len(), 1, "a bucket of two, capped at one");
        assert_eq!(
            entries[0].path.as_str(),
            "root/sub/ancient.bin",
            "the heaviest survives"
        );

        assert!(
            age_bucket_entries(&tree, root, NOW, 5, 0).expect("a root").is_empty(),
            "a limit of zero is an empty list, not the whole bucket"
        );
    }

    #[test]
    fn a_bucket_index_out_of_range_is_empty_rather_than_a_failure() {
        let (tree, root) = fixture();
        // A stale front end asking for a bucket this build does not have should
        // get "nothing there", not an error dialog.
        let entries = age_bucket_entries(&tree, root, NOW, AGE_BUCKET_COUNT, 10).expect("a root");
        assert!(entries.is_empty());
    }

    #[test]
    fn the_leaderboard_and_the_totals_agree_when_given_the_same_now() {
        let (tree, root) = fixture();
        let rows = age_buckets(&tree, root, NOW).expect("a root");

        for row in &rows {
            let bucket = usize::from(row.bucket);
            let entries = age_bucket_entries(&tree, root, NOW, bucket, 1_000).expect("a root");
            assert_eq!(
                u64::try_from(entries.len()).unwrap_or(u64::MAX),
                row.files,
                "bucket {bucket} lists a different number of files than it counted"
            );
            let listed: u64 = entries.iter().map(|entry| entry.allocated).sum();
            assert_eq!(
                listed, row.allocated,
                "bucket {bucket} lists different bytes than it counted"
            );
        }
    }
}
