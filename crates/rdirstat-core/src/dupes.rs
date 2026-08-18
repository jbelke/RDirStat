//! Duplicate **candidates**: files that *could* hold the same bytes, decided
//! from metadata alone.
//!
//! ## This is stage 1–2 of a five-stage pipeline, and it stops there
//!
//! docs/06-DATA.md#duplicate-detection specifies five stages:
//!
//! 1. collapse hard-linked names by `(device, inode)` within a scan;
//! 2. group regular files by logical size, discard unique-size groups;
//! 3. open without following symlinks and hash the first and last 64 KiB;
//! 4. full-hash only the groups that survive that;
//! 5. `fstat` before and after, and discard a hash whose file moved under it.
//!
//! **Only 1 and 2 live here, and this module never opens a file.** This crate is
//! I/O-free by construction — it owns the arena and the wire types and depends on
//! nothing that can produce a file descriptor — so stages 3–5, which are entirely
//! file I/O, belong to a crate that is allowed to do I/O.
//!
//! That is not a caveat to be footnoted; it is the definition of the output.
//! **Two files of the same size are not duplicates.** Every 4 KiB `.plist` on a
//! Mac is 4 KiB. Nothing below has compared one byte of any two files, so the
//! only true claim this module can make is "these are worth comparing", and every
//! name in it — module, functions, types, fields — says *candidate* so a caller
//! cannot accidentally render it as a finding.
//!
//! ## What the hard-link collapse can and cannot do without `dev`/`ino`
//!
//! [`Node`](crate::Node) deliberately does not store `(dev, ino)`: at the 69M
//! design profile two more `u64`s cost 1.1 GiB of resident memory, and identity
//! is scan-time state that belongs in the scanner's hard-link set and the catalog
//! row (see the "Deliberately not stored here" note on [`Node`](crate::Node)).
//! So this module cannot key anything by inode. What it has instead is the
//! scanner's *conclusion*, recorded per node:
//!
//! - [`flags::HARD_LINK`] — `nlink > 1`, the content has more than one name;
//! - [`flags::HARD_LINK_REPEAT`] — a later sighting of a `(dev, ino)` already
//!   counted in this scan, contributing zero bytes to every rollup.
//!
//! **Can** therefore be done, and is: drop every `HARD_LINK_REPEAT`. Two names
//! for one inode must never be offered as a two-member cluster promising the
//! file's size back, because deleting one name frees nothing but a directory
//! entry. Dropping the repeats also keeps this report reconciled with every other
//! total in the app, which all use
//! [`Node::contributed_size`](crate::Node::contributed_size).
//!
//! **Cannot** be done here, in order of how much it matters:
//!
//! - Prove two *retained* members are not the same inode. If a file's other name
//!   lives outside the scan root, only `HARD_LINK` is set — the scanner never saw
//!   the partner, so no repeat was ever marked. Such a member is flagged
//!   [`DuplicateCandidateMember::hard_linked`] and the UI must say that deleting
//!   it may free nothing.
//! - Say *where* a dropped repeat's counted-at path is. That mapping is the
//!   scanner's hard-link set, not the arena; [`Details`](crate::Details) carries
//!   `counted_at` for a single node, and this report deliberately does not fan
//!   out one such lookup per candidate.
//! - Notice that a repeat's first sighting was in a *different* subtree. This
//!   report walks the subtree it was asked about, but `HARD_LINK_REPEAT` is a
//!   scan-wide fact. An inode first met elsewhere is dropped here and appears in
//!   no cluster, which is the same policy the subtree's byte totals already use.
//! - Detect APFS clones. Two independent inodes can share every physical block.
//!   Only the content identifier queried in stage 5 can settle that, so a clone
//!   pair looks exactly like two ordinary files here.
//!
//! ## Empty files are dropped, not reported
//!
//! Every zero-length file on a volume is the same size, so they would form the
//! single largest "cluster" on the disk — and its potential recovery is
//! `0 × (n − 1) = 0` bytes. A group that is guaranteed to return nothing is
//! noise, not a finding. They are counted in
//! [`DuplicateCandidateReport::empty_files_skipped`] so the number is visible
//! rather than silently missing.
//!
//! ## Recovery is a range, and its floor is zero
//!
//! `size × (n − 1)` is what a naive duplicate finder prints, and docs/06 forbids
//! presenting it as guaranteed physical recovery. It is an **upper bound**, and
//! it is unreachable for two independent reasons:
//!
//! - the files may simply differ, in which case nothing is deletable at all —
//!   this is why the floor is zero and stays zero until contents are read;
//! - on APFS the copies may already be clones sharing physical blocks, so
//!   deleting one frees the metadata and nothing else.
//!
//! Both bounds are therefore returned as fields whose names contain
//! `potential_recovery_lower_bytes` / `potential_recovery_upper_bytes`. A field
//! called `recoverable` would be a lie that no amount of UI copy could undo.
//!
//! ## Everything is capped, and the cap is in the data
//!
//! A 12M-file volume has millions of same-size groups; a full enumeration is not
//! a report, it is a memory-exhaustion bug with a table around it. Three
//! ceilings, each surfaced so the UI can state what it is not showing:
//!
//! - [`MAX_TRACKED_SIZES`] distinct sizes in the counting pass. Sizes met after
//!   the table fills are not grouped at all; the files are counted in
//!   [`DuplicateCandidateReport::files_ungrouped`]. Counts for the clusters that
//!   *are* reported stay exact, because a size is either tracked from its first
//!   sighting or never tracked.
//! - [`MAX_DUPLICATE_CLUSTERS`] clusters returned, ranked by upper-bound
//!   recovery, with [`DuplicateCandidateReport::clusters_omitted`] naming the
//!   tail.
//! - [`MAX_CLUSTER_MEMBERS`] members listed per cluster, with
//!   [`DuplicateCandidateCluster::members_omitted`] naming the rest. The listed
//!   members are the lowest node ids — arena order, i.e. the order the scanner
//!   met them — so the same tree always yields the same list.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::id::NodeId;
use crate::node::{Node, flags};
use crate::tree::Tree;
use crate::wire::DisplayPath;

/// Ceiling on clusters returned by one [`duplicate_candidates`] call.
///
/// Two hundred rows is already more than a person will read; the ranking is what
/// makes the list useful, not its length.
pub const MAX_DUPLICATE_CLUSTERS: usize = 200;

/// Ceiling on members listed per cluster.
///
/// A cluster can hold a hundred thousand identically-sized icons. The count is
/// reported exactly; the listing is a sample.
pub const MAX_CLUSTER_MEMBERS: usize = 100;

/// Smallest useful member listing.
///
/// Listing one member of a candidate pair says nothing about what it is a
/// candidate *with*, so the member cap never clamps below two.
pub const MIN_CLUSTER_MEMBERS: usize = 2;

/// Ceiling on distinct logical sizes tracked while counting.
///
/// The counting table is the only structure in this module that grows with the
/// tree, so it is the only one that could blow the memory budget: one million
/// `(u64, u32)` entries is tens of megabytes, while an untracked table on a 69M
/// -entry volume could exceed a gigabyte. Reaching the ceiling degrades the
/// report to a floor and says so through
/// [`DuplicateCandidateReport::files_ungrouped`], rather than failing.
pub const MAX_TRACKED_SIZES: usize = 1 << 20;

/// One file that *might* be a duplicate of the others in its cluster.
///
/// The logical size is not repeated here: it is the cluster's grouping key and
/// identical for every member by construction, and two size columns on one row
/// is an invitation to display the wrong one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DuplicateCandidateMember {
    /// The arena node, so a row can be revealed or trashed.
    pub node: NodeId,
    /// Full path, escaped for display. Not action authority; see
    /// [`DisplayPath`].
    pub path: DisplayPath,
    /// Filesystem-allocated bytes. Kept per member because, unlike the logical
    /// size, it genuinely differs across a cluster — a sparse, compressed, or
    /// cloned copy allocates less. It is still not proof of independent
    /// physical allocation.
    pub allocated: u64,
    /// Modification time in whole Unix seconds.
    pub mtime: i64,
    /// Content category index.
    pub category: u8,
    /// `nlink > 1`: this content is reachable by more than one name, and at
    /// least one of those names was not seen in this scan. Deleting this member
    /// may free nothing at all.
    pub hard_linked: bool,
}

/// A set of files sharing one logical size — a group worth comparing, not a
/// group known to match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DuplicateCandidateCluster {
    /// The logical size every member reports, in bytes. The only thing they are
    /// known to have in common.
    pub logical_bytes: u64,
    /// How many candidate files carry this size. Exact, and independent of how
    /// many are listed in [`Self::members`].
    pub member_count: u32,
    /// The lowest node ids in the cluster, ascending — arena order, capped at
    /// the call's member limit.
    pub members: Vec<DuplicateCandidateMember>,
    /// Members not listed: the cap, plus any whose path could not be
    /// reconstructed.
    pub members_omitted: u32,
    /// Floor of the recovery range: **zero**, always, at this stage. No byte of
    /// any member has been read, so it is entirely possible that no two of them
    /// match and nothing here is deletable. The field exists so the UI renders a
    /// range instead of a single number that reads like a promise.
    pub potential_recovery_lower_bytes: u64,
    /// Ceiling of the recovery range: `logical_bytes × (member_count − 1)`,
    /// i.e. what would come back if every member but one were an identical,
    /// independently-stored copy. Never present this as recovered space; see the
    /// module docs.
    pub potential_recovery_upper_bytes: u64,
}

/// The bounded result of one candidate pass over a subtree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct DuplicateCandidateReport {
    /// Clusters, ranked by [`DuplicateCandidateCluster::potential_recovery_upper_bytes`]
    /// descending, then by size descending.
    pub clusters: Vec<DuplicateCandidateCluster>,
    /// Clusters found before the cluster cap was applied.
    pub clusters_found: u64,
    /// Clusters found but not returned, i.e. the ranked tail.
    pub clusters_omitted: u64,
    /// Candidate files across every cluster found, not only the returned ones.
    pub files_in_clusters: u64,
    /// Upper bound summed over every cluster **found**, so the headline number
    /// does not shrink when the list is truncated. Still an upper bound; see
    /// [`DuplicateCandidateCluster::potential_recovery_upper_bytes`].
    pub potential_recovery_upper_bytes: u64,
    /// Whether any file content was compared to produce this report.
    ///
    /// **Always `false` here** — stages 3–5 of the pipeline do not exist yet.
    /// It is a field rather than a constant so the UI branches on data instead
    /// of on a hard-coded assumption, and starts telling the truth on its own
    /// the day content hashing lands.
    pub content_verified: bool,
    /// The cluster cap actually applied, after clamping.
    pub cluster_limit: u32,
    /// The per-cluster member cap actually applied, after clamping.
    pub member_limit: u32,
    /// Zero-length files skipped. Reported so their absence is visible: every
    /// one of them is the same size and none of them can recover a byte.
    pub empty_files_skipped: u64,
    /// Repeated hard-link names skipped — a second name for content already
    /// counted, which recovers nothing when deleted.
    pub hard_link_repeats_skipped: u64,
    /// Files not grouped because [`MAX_TRACKED_SIZES`] distinct sizes were
    /// already being tracked. Non-zero means clusters may be **missing**; the
    /// ones reported are still exact.
    pub files_ungrouped: u64,
}

/// Why a file was not counted as a candidate, or its size if it was.
///
/// The reasons are ordered: a zero-length hard-link repeat is reported as a
/// repeat, because that is the stronger statement about the bytes.
enum Candidacy {
    /// A regular file with a non-zero size, in bytes.
    Eligible(u64),
    /// A second name for content already counted in this scan.
    HardLinkRepeat,
    /// A zero-length regular file.
    Empty,
    /// A directory, symlink, socket, device, or unclassifiable entry. Not
    /// tallied: "the subtree contains directories" is not a finding.
    Ineligible,
}

/// Classifies one node. Single source of truth, so the counting pass and the
/// listing pass can never disagree about what a candidate is.
fn candidacy(node: &Node) -> Candidacy {
    if !node.kind().is_file() {
        return Candidacy::Ineligible;
    }
    if node.has_flags(flags::HARD_LINK_REPEAT) {
        return Candidacy::HardLinkRepeat;
    }
    if node.size == 0 {
        return Candidacy::Empty;
    }
    // The raw `size`, not `contributed_size`, precisely because repeats were
    // already rejected above: for everything that reaches this line the two are
    // equal, and grouping on a zeroed contribution would fold every repeat into
    // the empty-file group this module discards.
    Candidacy::Eligible(node.size)
}

/// Files skipped by [`walk_files`], by reason.
#[derive(Clone, Copy, Debug, Default)]
struct SkipTally {
    empty_files: u64,
    hard_link_repeats: u64,
}

/// Visits every candidate file under `start`, iteratively.
///
/// Explicit stack, never recursion — a 4096-deep chain is a real input and a
/// stack overflow is not a diagnosable failure. One implementation, used by both
/// passes, so the traversal order and the budget backstop cannot drift apart.
fn walk_files(tree: &Tree, start: NodeId, mut visit: impl FnMut(NodeId, u64)) -> SkipTally {
    let mut tally = SkipTally::default();
    let mut stack = vec![start];
    // The arena is finite and acyclic by `Tree`'s freeze-time validation, so
    // this is a backstop against a tree that somehow escaped it, not an
    // expected limit.
    let mut budget = tree.len().saturating_mul(2).saturating_add(16);

    while let Some(id) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;

        let Some(node) = tree.node(id) else { continue };
        match candidacy(node) {
            Candidacy::Eligible(size) => visit(id, size),
            Candidacy::HardLinkRepeat => tally.hard_link_repeats = tally.hard_link_repeats.saturating_add(1),
            Candidacy::Empty => tally.empty_files = tally.empty_files.saturating_add(1),
            Candidacy::Ineligible => {}
        }

        stack.extend(tree.children(id));
    }

    tally
}

/// One cluster under construction: the counted truth plus a bounded sample.
struct Picked {
    size: u64,
    total: u32,
    upper: u64,
    kept: Vec<NodeId>,
    /// Raw id of the worst kept member once the sample is full; `u32::MAX` until
    /// then. See [`sample_members`].
    ceiling: u32,
}

/// The result of the counting pass.
struct SizeCensus {
    /// How many candidate files carry each tracked logical size.
    counts: HashMap<u64, u32>,
    /// Files the pass declined to consider, by reason.
    skipped: SkipTally,
    /// Files whose size arrived after [`MAX_TRACKED_SIZES`] was reached.
    files_ungrouped: u64,
}

/// Pass one: how many candidate files carry each logical size.
///
/// Counts only; node ids are deliberately not remembered here, because that is
/// the one structure that would grow with the number of *files* rather than with
/// the number of distinct sizes.
fn count_by_size(tree: &Tree, start: NodeId) -> SizeCensus {
    let mut counts: HashMap<u64, u32> = HashMap::new();
    let mut files_ungrouped = 0_u64;
    let skipped = walk_files(tree, start, |_id, size| {
        if let Some(count) = counts.get_mut(&size) {
            *count = count.saturating_add(1);
        } else if counts.len() < MAX_TRACKED_SIZES {
            counts.insert(size, 1);
        } else {
            // A size first met after the table filled is never tracked at all,
            // so the counts above stay exact and only whole clusters can go
            // missing.
            files_ungrouped = files_ungrouped.saturating_add(1);
        }
    });
    SizeCensus {
        counts,
        skipped,
        files_ungrouped,
    }
}

/// Pass two: fill each selected cluster with a bounded sample of its members.
///
/// The sample is the lowest node ids — arena order, i.e. the order the scanner
/// met them — so it is a function of the tree rather than of the traversal, and
/// two runs over one snapshot list the same rows.
fn sample_members(tree: &Tree, start: NodeId, index: &HashMap<u64, usize>, picked: &mut [Picked], member_limit: usize) {
    walk_files(tree, start, |id, size| {
        let Some(slot) = index.get(&size).copied().and_then(|at| picked.get_mut(at)) else {
            return;
        };
        if slot.kept.len() < member_limit {
            slot.kept.push(id);
            if slot.kept.len() == member_limit {
                slot.kept.sort_unstable();
                slot.ceiling = slot.kept.last().map_or(u32::MAX, |node| node.raw());
            }
        } else if id.raw() < slot.ceiling {
            // The ceiling check is what keeps a hundred-thousand-member cluster
            // from re-sorting on every sighting: once the sample is full, only an
            // id that beats the worst kept one touches the vector at all.
            slot.kept.push(id);
            slot.kept.sort_unstable();
            slot.kept.truncate(member_limit);
            slot.ceiling = slot.kept.last().map_or(u32::MAX, |node| node.raw());
        }
    });
}

/// Turns sampled node ids into wire members, resolving one path per listed row.
///
/// Bounded by `cluster_limit × member_limit` path reconstructions, which is why
/// paths are resolved here and not during either walk.
fn resolve_clusters(tree: &Tree, picked: Vec<Picked>) -> Vec<DuplicateCandidateCluster> {
    let mut clusters = Vec::with_capacity(picked.len());
    let mut scratch = Vec::new();
    for mut slot in picked {
        slot.kept.sort_unstable();
        let mut members = Vec::with_capacity(slot.kept.len());
        for id in slot.kept {
            let Some(node) = tree.node(id) else { continue };
            scratch.clear();
            // A path that cannot be reconstructed is dropped rather than shown
            // blank: a row naming no file is worse than a shorter list, and this
            // row offers a Trash action.
            if tree.path_bytes(id, &mut scratch).is_err() {
                continue;
            }
            members.push(DuplicateCandidateMember {
                node: id,
                path: DisplayPath::from_bytes(&scratch),
                allocated: node.alloc,
                mtime: node.mtime,
                category: node.category,
                hard_linked: node.has_flags(flags::HARD_LINK),
            });
        }
        let listed = u32::try_from(members.len()).unwrap_or(u32::MAX);
        clusters.push(DuplicateCandidateCluster {
            logical_bytes: slot.size,
            member_count: slot.total,
            members,
            members_omitted: slot.total.saturating_sub(listed),
            potential_recovery_lower_bytes: 0,
            potential_recovery_upper_bytes: slot.upper,
        });
    }
    clusters
}

/// Groups the files under `root` by logical size and returns the same-size
/// clusters, ranked by how much space they could *possibly* recover.
///
/// The result is candidates. Nothing is opened, nothing is hashed, and a cluster
/// is only the claim that its members share a size — read the module docs before
/// wording any UI around it.
///
/// `max_clusters` is clamped to `1..=`[`MAX_DUPLICATE_CLUSTERS`] and
/// `max_members` to [`MIN_CLUSTER_MEMBERS`]`..=`[`MAX_CLUSTER_MEMBERS`]; zero
/// means "use the ceiling". Clamping rather than erroring matches
/// [`clamp_page_limit`](crate::clamp_page_limit): a caller that asks for one row
/// too many should get a page, not a failure.
///
/// Costs two passes over the subtree. The alternative — remembering every
/// candidate node id during the counting pass — is unbounded in exactly the case
/// this report exists for.
///
/// Returns `None` if `root` is not a node in this tree.
#[must_use]
pub fn duplicate_candidates(
    tree: &Tree,
    root: NodeId,
    max_clusters: usize,
    max_members: usize,
) -> Option<DuplicateCandidateReport> {
    // A virtual `<Files>` group has no arena node of its own; report on its
    // owner, the same way the size bands do.
    let start = root.group_owner().unwrap_or(root);
    tree.node(start)?;

    let cluster_limit = if max_clusters == 0 {
        MAX_DUPLICATE_CLUSTERS
    } else {
        max_clusters.min(MAX_DUPLICATE_CLUSTERS)
    };
    let member_limit = if max_members == 0 {
        MAX_CLUSTER_MEMBERS
    } else {
        max_members.clamp(MIN_CLUSTER_MEMBERS, MAX_CLUSTER_MEMBERS)
    };

    let census = count_by_size(tree, start);

    // A size seen once is not a candidate for anything: this is stage 2's
    // "discard unique-size groups", and it is what makes the result small.
    let mut ranked: Vec<(u64, u64, u32)> = census
        .counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(size, count)| (size.saturating_mul(u64::from(count - 1)), size, count))
        .collect();

    let clusters_found = u64::try_from(ranked.len()).unwrap_or(u64::MAX);
    let files_in_clusters = ranked
        .iter()
        .fold(0_u64, |sum, (_, _, count)| sum.saturating_add(u64::from(*count)));
    let potential_recovery_upper_bytes = ranked
        .iter()
        .fold(0_u64, |sum, (upper, _, _)| sum.saturating_add(*upper));

    // Rank by what the user came for — space — and not by file size: ten copies
    // of a 3 MB asset outrank two copies of a 10 MB one. Size breaks the tie, and
    // since sizes are unique keys the order is total and reproducible.
    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    ranked.truncate(cluster_limit);
    let clusters_omitted = clusters_found.saturating_sub(u64::try_from(ranked.len()).unwrap_or(u64::MAX));

    // The size -> cluster index the second pass looks every candidate up in.
    // Bounded by `cluster_limit`, which is why the listing pass costs nothing
    // per file that is not in a reported cluster.
    let mut index: HashMap<u64, usize> = HashMap::with_capacity(ranked.len());
    let mut picked: Vec<Picked> = Vec::with_capacity(ranked.len());
    for (upper, size, total) in ranked {
        index.insert(size, picked.len());
        picked.push(Picked {
            size,
            total,
            upper,
            kept: Vec::new(),
            ceiling: u32::MAX,
        });
    }

    sample_members(tree, start, &index, &mut picked, member_limit);

    Some(DuplicateCandidateReport {
        clusters: resolve_clusters(tree, picked),
        clusters_found,
        clusters_omitted,
        files_in_clusters,
        potential_recovery_upper_bytes,
        content_verified: false,
        cluster_limit: u32::try_from(cluster_limit).unwrap_or(u32::MAX),
        member_limit: u32::try_from(member_limit).unwrap_or(u32::MAX),
        empty_files_skipped: census.skipped.empty_files,
        hard_link_repeats_skipped: census.skipped.hard_link_repeats,
        files_ungrouped: census.files_ungrouped,
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;
    use crate::dirs::DirTotals;
    use crate::node::Kind;
    use crate::tree::TreeBuilder;

    /// A builder wrapper, because every fixture below needs the same four calls
    /// and the interesting part of each test is its shape, not its plumbing.
    struct Fixture {
        builder: TreeBuilder,
        root: NodeId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut builder = TreeBuilder::new();
            let name = builder.intern(b"root").expect("interns");
            let root = builder.push_node(Node::directory(name, 0)).expect("pushes");
            builder.register_directory(root, DirTotals::EMPTY).expect("registers");
            Self { builder, root }
        }

        fn dir(&mut self, parent: NodeId, name: &[u8]) -> NodeId {
            let reference = self.builder.intern(name).expect("interns");
            let id = self
                .builder
                .push_child(parent, Node::directory(reference, 0))
                .expect("links");
            self.builder
                .register_directory(id, DirTotals::EMPTY)
                .expect("registers");
            id
        }

        fn file(&mut self, parent: NodeId, name: &[u8], bytes: u64) -> NodeId {
            self.leaf(parent, name, bytes, Kind::File, flags::NONE)
        }

        fn leaf(&mut self, parent: NodeId, name: &[u8], bytes: u64, kind: Kind, bits: u16) -> NodeId {
            let reference = self.builder.intern(name).expect("interns");
            let node = Node::leaf(reference, kind, bytes, bytes, 0).with_flags(bits);
            self.builder.push_child(parent, node).expect("links")
        }

        fn finish(self) -> (Tree, NodeId) {
            (self.builder.finish().expect("valid"), self.root)
        }
    }

    fn report(tree: &Tree, root: NodeId) -> DuplicateCandidateReport {
        duplicate_candidates(tree, root, 0, 0).expect("a root")
    }

    #[test]
    fn files_of_equal_size_cluster_and_unique_sizes_are_discarded() {
        let mut fixture = Fixture::new();
        fixture.file(fixture.root, b"a.bin", 4096);
        fixture.file(fixture.root, b"b.bin", 4096);
        fixture.file(fixture.root, b"lonely.bin", 8192);
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        assert_eq!(report.clusters.len(), 1, "one size repeats, one does not");
        assert_eq!(report.clusters[0].logical_bytes, 4096);
        assert_eq!(report.clusters[0].member_count, 2);
        assert_eq!(report.files_in_clusters, 2, "the unique size contributes no candidates");
    }

    #[test]
    fn a_cluster_is_never_reported_as_verified_because_nothing_was_read() {
        let mut fixture = Fixture::new();
        fixture.file(fixture.root, b"a.bin", 4096);
        fixture.file(fixture.root, b"b.bin", 4096);
        let (tree, root) = fixture.finish();

        assert!(
            !report(&tree, root).content_verified,
            "this module opens no file, so it may never claim otherwise"
        );
    }

    #[test]
    fn recovery_is_a_range_whose_floor_stays_zero_until_contents_are_read() {
        let mut fixture = Fixture::new();
        for name in [b"a.bin".as_slice(), b"b.bin", b"c.bin"] {
            fixture.file(fixture.root, name, 1 << 20);
        }
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        let cluster = &report.clusters[0];
        // Three same-size files: at most two are removable, and possibly none of
        // them, because no two have been compared.
        assert_eq!(cluster.potential_recovery_upper_bytes, 2 << 20);
        assert_eq!(
            cluster.potential_recovery_lower_bytes, 0,
            "same size is not same content; the floor cannot rise without a hash"
        );
    }

    #[test]
    fn a_hard_link_repeat_is_not_counted_as_recoverable_space() {
        let mut fixture = Fixture::new();
        // Two names for one inode, plus one genuinely separate file of the same
        // size. The honest cluster is the pair {original, other}, worth one
        // file back — not a trio worth two.
        fixture.leaf(fixture.root, b"original.bin", 1 << 20, Kind::File, flags::HARD_LINK);
        fixture.leaf(
            fixture.root,
            b"second-name.bin",
            1 << 20,
            Kind::File,
            flags::HARD_LINK | flags::HARD_LINK_REPEAT,
        );
        fixture.file(fixture.root, b"other.bin", 1 << 20);
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        assert_eq!(report.hard_link_repeats_skipped, 1);
        assert_eq!(report.clusters.len(), 1);
        let cluster = &report.clusters[0];
        assert_eq!(cluster.member_count, 2, "the repeat is a second name, not a copy");
        assert_eq!(
            cluster.potential_recovery_upper_bytes,
            1 << 20,
            "deleting the second name of an inode frees a directory entry, not a megabyte"
        );
        assert_eq!(report.potential_recovery_upper_bytes, 1 << 20);
        assert!(
            !cluster
                .members
                .iter()
                .any(|member| member.path.as_str().contains("second-name")),
            "a repeat must not be offered for deletion"
        );
    }

    #[test]
    fn a_surviving_hard_link_is_flagged_because_deleting_it_may_free_nothing() {
        let mut fixture = Fixture::new();
        // `nlink > 1` with no repeat seen: the partner name lives outside the
        // scan root, so the scanner never marked one. All this module can do is
        // say so.
        fixture.leaf(fixture.root, b"linked.bin", 4096, Kind::File, flags::HARD_LINK);
        fixture.file(fixture.root, b"plain.bin", 4096);
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        let flagged: Vec<bool> = report.clusters[0]
            .members
            .iter()
            .map(|member| member.hard_linked)
            .collect();
        assert_eq!(flagged, vec![true, false]);
    }

    #[test]
    fn empty_files_are_skipped_because_deleting_one_recovers_nothing() {
        let mut fixture = Fixture::new();
        for name in [b"e1".as_slice(), b"e2", b"e3"] {
            fixture.file(fixture.root, name, 0);
        }
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        assert!(
            report.clusters.is_empty(),
            "the biggest same-size group on a disk is worth nothing"
        );
        assert_eq!(report.empty_files_skipped, 3, "skipped, but visibly so");
    }

    #[test]
    fn clusters_rank_by_possible_recovery_rather_than_by_file_size() {
        let mut fixture = Fixture::new();
        // Two 10 MiB files -> 10 MiB upper bound.
        fixture.file(fixture.root, b"big-a.bin", 10 << 20);
        fixture.file(fixture.root, b"big-b.bin", 10 << 20);
        // Ten 3 MiB files -> 27 MiB upper bound, from smaller files.
        let many = fixture.dir(fixture.root, b"many");
        for index in 0..10_u8 {
            fixture.file(many, &[b'm', b'0' + index], 3 << 20);
        }
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        assert_eq!(report.clusters.len(), 2);
        assert_eq!(
            report.clusters[0].logical_bytes,
            3 << 20,
            "27 MiB of upside outranks 10"
        );
        assert_eq!(report.clusters[0].potential_recovery_upper_bytes, 27 << 20);
        assert_eq!(report.clusters[1].potential_recovery_upper_bytes, 10 << 20);
        assert_eq!(report.potential_recovery_upper_bytes, 37 << 20);
    }

    #[test]
    fn the_cluster_cap_truncates_the_ranked_tail_and_reports_what_it_dropped() {
        let mut fixture = Fixture::new();
        for size in 1..=5_u64 {
            let dir = fixture.dir(fixture.root, &[b'd', b'0' + u8::try_from(size).expect("small")]);
            fixture.file(dir, b"a", size * 4096);
            fixture.file(dir, b"b", size * 4096);
        }
        let (tree, root) = fixture.finish();

        let report = duplicate_candidates(&tree, root, 2, 0).expect("a root");
        assert_eq!(report.clusters.len(), 2);
        assert_eq!(report.clusters_found, 5);
        assert_eq!(report.clusters_omitted, 3);
        assert_eq!(report.cluster_limit, 2, "the cap travels with the data");
        // The headline total describes everything found, so it does not shrink
        // when the list is shortened.
        assert_eq!(report.potential_recovery_upper_bytes, (1 + 2 + 3 + 4 + 5) * 4096);
        assert_eq!(
            report.clusters[0].logical_bytes,
            5 * 4096,
            "the heaviest survives the cap"
        );
    }

    #[test]
    fn the_member_cap_lists_a_prefix_and_counts_the_rest_as_omitted() {
        let mut fixture = Fixture::new();
        for index in 0..8_u8 {
            fixture.file(fixture.root, &[b'f', b'0' + index], 4096);
        }
        let (tree, root) = fixture.finish();

        let report = duplicate_candidates(&tree, root, 0, 3).expect("a root");
        let cluster = &report.clusters[0];
        assert_eq!(cluster.member_count, 8, "the count is the truth");
        assert_eq!(cluster.members.len(), 3, "the listing is a sample");
        assert_eq!(cluster.members_omitted, 5);
        assert_eq!(report.member_limit, 3);
        // Lowest node ids, ascending: the sample is a function of the tree, not
        // of the order the stack happened to pop.
        let ids: Vec<u32> = cluster.members.iter().map(|member| member.node.raw()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        assert_eq!(
            cluster.potential_recovery_upper_bytes,
            7 * 4096,
            "the bound follows the count, never the sample"
        );
    }

    #[test]
    fn a_member_listing_of_one_would_be_useless_so_the_cap_never_goes_below_two() {
        let mut fixture = Fixture::new();
        fixture.file(fixture.root, b"a.bin", 4096);
        fixture.file(fixture.root, b"b.bin", 4096);
        fixture.file(fixture.root, b"c.bin", 4096);
        let (tree, root) = fixture.finish();

        let report = duplicate_candidates(&tree, root, 0, 1).expect("a root");
        assert_eq!(report.member_limit, u32::try_from(MIN_CLUSTER_MEMBERS).expect("small"));
        assert_eq!(report.clusters[0].members.len(), 2);
    }

    #[test]
    fn only_regular_files_are_candidates() {
        let mut fixture = Fixture::new();
        // Same size on a symlink, a device, and a directory's own entry: none of
        // them is a file whose bytes could be duplicated.
        fixture.leaf(fixture.root, b"link", 4096, Kind::Symlink, flags::NONE);
        fixture.leaf(fixture.root, b"dev", 4096, Kind::BlockDevice, flags::NONE);
        fixture.dir(fixture.root, b"sub");
        fixture.file(fixture.root, b"only.bin", 4096);
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        assert!(report.clusters.is_empty(), "one regular file is not a cluster");
        assert_eq!(report.empty_files_skipped, 0, "a symlink is not an empty file");
    }

    #[test]
    fn nested_directories_are_walked_without_recursion() {
        let mut fixture = Fixture::new();
        let mut cursor = fixture.root;
        for depth in 0..64_u8 {
            cursor = fixture.dir(cursor, &[b'd', b'0' + depth % 10]);
            fixture.file(cursor, b"same.bin", 4096);
        }
        let (tree, root) = fixture.finish();

        let report = report(&tree, root);
        assert_eq!(report.clusters[0].member_count, 64);
    }

    #[test]
    fn a_virtual_files_group_reports_on_the_directory_that_owns_it() {
        let mut fixture = Fixture::new();
        fixture.file(fixture.root, b"a.bin", 4096);
        fixture.file(fixture.root, b"b.bin", 4096);
        let (tree, root) = fixture.finish();

        let group = NodeId::virtual_group_of(root).expect("a group id");
        assert_eq!(
            duplicate_candidates(&tree, group, 0, 0),
            duplicate_candidates(&tree, root, 0, 0)
        );
    }

    #[test]
    fn an_unknown_node_has_no_report() {
        let mut fixture = Fixture::new();
        fixture.file(fixture.root, b"a.bin", 4096);
        let (tree, _) = fixture.finish();

        assert!(duplicate_candidates(&tree, NodeId::from_raw(9_999), 0, 0).is_none());
    }
}
