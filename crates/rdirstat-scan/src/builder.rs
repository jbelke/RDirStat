//! The single-writer builder: the only thing that assigns [`NodeId`]s.
//!
//! Readers hand it batches; it owns the arena, the paths, the hard-link set,
//! the counters, and the error log. That is the entire concurrency design —
//! no `Arc<Mutex<Tree>>`, no per-node atomic
//! (docs/01-ARCHITECTURE.md#scan-concurrency-and-ownership).
//!
//! Every traversal rule from docs/02-SCANNER.md#traversal-rules is decided
//! here, in one place, so the sequential and parallel schedulers cannot drift
//! apart: they differ only in *when* a directory is read, never in what its
//! entries mean.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rdirstat_core::{
    ArenaError, DirTotals, DisplayPath, ErrorClass, ErrorClassCount, Kind, MAX_DETAILED_ERRORS, MAX_TREE_DEPTH,
    NameRef, Node, NodeId, Operation, ScanCounts, ScanError, ScanTotals, Tree, TreeBuilder, flags,
};

use crate::categorize::{Categorizer, is_package_name};
use crate::entry::{EntryError, RawEntryBatch, SmallName};
use crate::exclude::{ExclusionSet, Verdict};
use crate::hardlink::HardLinkSet;
use crate::progress::{ArenaFootprint, Counters};
use crate::reader::{DirHandle, ReadDirError, SF_FIRMLINK};
use crate::std_reader::BROKEN_SYMLINK_PROBE;

/// How many excluded roots are listed in full. Beyond this the count in
/// [`ScanCounts::excluded_paths`] still grows; only the list stops.
pub(crate) const MAX_LISTED_EXCLUSIONS: usize = MAX_DETAILED_ERRORS;

/// What one completed scan produced.
#[derive(Debug)]
pub(crate) struct Artifacts {
    pub(crate) tree: Tree,
    pub(crate) counts: ScanCounts,
    pub(crate) totals: ScanTotals,
    pub(crate) errors: Vec<ScanError>,
    pub(crate) error_counts: Vec<ErrorClassCount>,
    pub(crate) excluded_roots: Vec<DisplayPath>,
    pub(crate) mutations: u64,
}

/// The arena owner.
pub(crate) struct ScanBuilder<'a> {
    tree: TreeBuilder,
    hard_links: HardLinkSet,
    exclusions: &'a ExclusionSet,
    categorizer: &'a dyn Categorizer,
    counters: &'a Counters,
    cross_filesystems: bool,
    count_hard_links_once: bool,
    root_device: u64,
    root_prefix_len: usize,
    counts: ScanCounts,
    errors: Vec<ScanError>,
    error_counts: Vec<ErrorClassCount>,
    excluded_roots: Vec<DisplayPath>,
    mutations: u64,
}

impl core::fmt::Debug for ScanBuilder<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ScanBuilder")
            .field("nodes", &self.tree.len())
            .field("directories", &self.tree.directory_count())
            .field("errors", &self.errors.len())
            .finish_non_exhaustive()
    }
}

/// Everything the builder needs that is fixed for the whole scan.
#[derive(Clone, Copy)]
pub(crate) struct BuilderConfig<'a> {
    pub(crate) exclusions: &'a ExclusionSet,
    pub(crate) categorizer: &'a dyn Categorizer,
    pub(crate) counters: &'a Counters,
    pub(crate) cross_filesystems: bool,
    pub(crate) count_hard_links_once: bool,
    pub(crate) root_device: u64,
    pub(crate) root_mtime: i64,
}

impl<'a> ScanBuilder<'a> {
    /// Creates the arena and pushes the scan root as node zero.
    ///
    /// The root's name is the whole root path, because
    /// [`Tree::path_bytes`](rdirstat_core::Tree::path_bytes) reconstructs every
    /// path from it.
    pub(crate) fn new(root: &Path, config: BuilderConfig<'a>) -> Result<Self, ScanError> {
        let mut tree = TreeBuilder::new();
        let root_bytes = path_bytes(root);
        let name = tree.intern(root_bytes).map_err(ScanError::Arena)?;
        let id = tree
            .push_node(Node::directory(name, config.root_mtime))
            .map_err(ScanError::Arena)?;
        tree.register_directory(
            id,
            DirTotals {
                latest_mtime: config.root_mtime,
                ..DirTotals::EMPTY
            },
        )
        .map_err(ScanError::Arena)?;

        Counters::add(&config.counters.observed_entries, 1);
        Counters::add(&config.counters.retained_nodes, 1);
        Counters::add(&config.counters.directories, 1);

        Ok(Self {
            tree,
            hard_links: HardLinkSet::new(),
            exclusions: config.exclusions,
            categorizer: config.categorizer,
            counters: config.counters,
            cross_filesystems: config.cross_filesystems,
            count_hard_links_once: config.count_hard_links_once,
            root_device: config.root_device,
            root_prefix_len: root_bytes.len(),
            counts: ScanCounts {
                observed_entries: 1,
                directories: 1,
                ..ScanCounts::default()
            },
            errors: Vec::new(),
            error_counts: Vec::new(),
            excluded_roots: Vec::new(),
            mutations: 0,
        })
    }

    /// A handle for the scan root, to seed a scheduler.
    pub(crate) fn root_handle(&self, root: &Path) -> DirHandle {
        DirHandle::new(root.to_path_buf(), self.tree.root(), self.root_device, 0)
    }

    /// Folds one batch into the arena, appending any directory that should be
    /// descended into `out`.
    ///
    /// # Errors
    ///
    /// Only the fatal arena failures: a full arena or a name blob past its
    /// 48-bit offset field. Every filesystem-level problem is recorded and the
    /// scan continues.
    pub(crate) fn absorb_batch(
        &mut self,
        dir: &DirHandle,
        batch: RawEntryBatch<'_>,
        out: &mut Vec<DirHandle>,
    ) -> Result<(), ScanError> {
        for failure in batch.errors() {
            self.absorb_entry_error(dir, failure);
        }
        for entry in batch.entries() {
            if entry.name.is_dot_entry() || entry.name.is_empty() {
                continue;
            }
            self.counts.observed_entries = self.counts.observed_entries.saturating_add(1);
            Counters::add(&self.counters.observed_entries, 1);

            if entry.kind == Kind::Directory {
                self.absorb_directory_entry(dir, entry, out)?;
            } else {
                self.absorb_leaf_entry(dir, entry)?;
            }
        }
        Ok(())
    }

    fn absorb_directory_entry(
        &mut self,
        dir: &DirHandle,
        entry: &crate::entry::RawEntry,
        out: &mut Vec<DirHandle>,
    ) -> Result<(), ScanError> {
        let name = entry.name.as_bytes();
        let child_path = join_bytes(dir.path(), name);
        let child_bytes = path_bytes(&child_path);
        let relative = relative_bytes(child_bytes, self.root_prefix_len);

        let excluded = self.exclusions.verdict(name, relative) == Verdict::Skip;
        let crosses = entry.dev != self.root_device;
        let firmlink = entry.platform_flags & SF_FIRMLINK != 0;
        let too_deep = dir.depth().saturating_add(1) >= MAX_TREE_DEPTH;

        let mut bits = flags::NONE;
        if excluded {
            bits |= flags::EXCLUDED;
        }
        if crosses {
            bits |= flags::MOUNT_POINT;
        }
        if firmlink {
            bits |= flags::FIRMLINK;
        }
        if too_deep {
            bits |= flags::INCOMPLETE;
        }
        if is_package_name(name) {
            bits |= flags::PACKAGE;
        }

        let Some(name_ref) = self.intern(name, child_bytes)? else {
            return Ok(());
        };
        let category = self.categorizer.categorize(name, Kind::Directory, bits);
        let node = Node::directory(name_ref, entry.mtime)
            .with_flags(bits)
            .with_category(category);
        let id = self.push_child(dir.node(), node)?;
        self.tree
            .register_directory(
                id,
                DirTotals {
                    latest_mtime: entry.mtime,
                    ..DirTotals::EMPTY
                },
            )
            .map_err(ScanError::Arena)?;

        self.counts.directories = self.counts.directories.saturating_add(1);
        Counters::add(&self.counters.directories, 1);

        if excluded {
            self.counts.excluded_paths = self.counts.excluded_paths.saturating_add(1);
            if self.excluded_roots.len() < MAX_LISTED_EXCLUSIONS {
                self.excluded_roots.push(DisplayPath::from_bytes(child_bytes));
            }
            return Ok(());
        }
        // A firmlink is never descended during a root-volume scan: doing so
        // walks the data volume a second time. A device boundary is not crossed
        // unless the option says so, and the marker node stays either way.
        if firmlink || too_deep || (crosses && !self.cross_filesystems) {
            return Ok(());
        }
        out.push(DirHandle::new(child_path, id, entry.dev, dir.depth().saturating_add(1)));
        Ok(())
    }

    fn absorb_leaf_entry(&mut self, dir: &DirHandle, entry: &crate::entry::RawEntry) -> Result<(), ScanError> {
        let name = entry.name.as_bytes();
        let mut bits = flags::NONE;
        let mut size = entry.size;
        let mut alloc = entry.alloc;

        match entry.kind {
            Kind::File => {
                if entry.nlink > 1 {
                    bits |= flags::HARD_LINK;
                    if self.count_hard_links_once && !self.hard_links.observe(entry.dev, entry.ino) {
                        bits |= flags::HARD_LINK_REPEAT;
                        self.counts.hard_link_repeats = self.counts.hard_link_repeats.saturating_add(1);
                    }
                }
                if alloc < size {
                    bits |= flags::SPARSE;
                }
                if entry.mode & 0o111 != 0 {
                    bits |= flags::EXECUTABLE;
                }
                self.counts.files = self.counts.files.saturating_add(1);
            }
            Kind::Symlink => {
                if entry.platform_flags & BROKEN_SYMLINK_PROBE != 0 {
                    bits |= flags::BROKEN_SYMLINK;
                }
                self.counts.symlinks = self.counts.symlinks.saturating_add(1);
            }
            Kind::Socket | Kind::Fifo | Kind::CharDevice | Kind::BlockDevice => {
                // Special kinds are leaves with zero data contribution and
                // their contents are never opened. `st_size` on a device node
                // is not user data.
                size = 0;
                alloc = 0;
                self.counts.special = self.counts.special.saturating_add(1);
            }
            _ => {
                size = 0;
                alloc = 0;
            }
        }

        let Some(name_ref) = self.intern(name, name)? else {
            return Ok(());
        };
        let category = self.categorizer.categorize(name, entry.kind, bits);
        let node = Node::leaf(name_ref, entry.kind, size, alloc, entry.mtime)
            .with_flags(bits)
            .with_category(category);
        // Hard-link policy lives only in `contributed_*`; it is never
        // reimplemented here.
        let contributed_size = node.contributed_size();
        let contributed_alloc = node.contributed_alloc();
        self.push_child(dir.node(), node)?;

        if let Some(totals) = self.tree.dir_totals_mut(dir.node()) {
            totals.absorb_direct_file(contributed_size, contributed_alloc, entry.mtime);
            totals.observed_entries = totals.observed_entries.saturating_add(1);
            totals.retained_nodes = totals.retained_nodes.saturating_add(1);
        }
        Counters::add(&self.counters.logical_bytes, contributed_size);
        Counters::add(&self.counters.allocated_bytes, contributed_alloc);
        Ok(())
    }

    fn absorb_entry_error(&mut self, dir: &DirHandle, failure: &EntryError) {
        self.counts.observed_entries = self.counts.observed_entries.saturating_add(1);
        Counters::add(&self.counters.observed_entries, 1);
        if let Some(totals) = self.tree.dir_totals_mut(dir.node()) {
            totals.observed_entries = totals.observed_entries.saturating_add(1);
        }
        let path = child_display(dir.path(), &failure.name);
        let error = match failure.class {
            ErrorClass::NotFound => {
                self.note_mutation();
                ScanError::Vanished {
                    path,
                    operation: failure.operation,
                }
            }
            ErrorClass::PermissionDenied => ScanError::PermissionDenied {
                path,
                operation: failure.operation,
                os_code: failure.os_code,
            },
            ErrorClass::InvalidName => ScanError::InvalidName { path },
            class => ScanError::Io {
                path,
                operation: failure.operation,
                class,
                os_code: failure.os_code,
            },
        };
        self.record(error);
    }

    /// Records the outcome of reading one directory.
    ///
    /// `EACCES`/`EPERM` mark the node unreadable and the scan continues; a
    /// per-directory failure never leaves the scan as a `Result::Err`.
    pub(crate) fn finish_directory(&mut self, dir: &DirHandle, error: Option<&ReadDirError>) {
        let Some(ReadDirError::Os {
            operation,
            class,
            os_code,
        }) = error
        else {
            return;
        };
        let path = DisplayPath::from_bytes(path_bytes(dir.path()));
        match class {
            ErrorClass::PermissionDenied => {
                self.mark(dir.node(), flags::UNREADABLE);
                if let Some(totals) = self.tree.dir_totals_mut(dir.node()) {
                    totals.unreadable = totals.unreadable.saturating_add(1);
                }
                self.counts.unreadable_dirs = self.counts.unreadable_dirs.saturating_add(1);
                self.record(ScanError::PermissionDenied {
                    path,
                    operation: *operation,
                    os_code: *os_code,
                });
            }
            ErrorClass::NotFound => {
                self.mark(dir.node(), flags::MUTATED | flags::INCOMPLETE);
                self.note_mutation();
                self.record(ScanError::Vanished {
                    path,
                    operation: *operation,
                });
            }
            other => {
                self.mark(dir.node(), flags::INCOMPLETE);
                self.record(ScanError::Io {
                    path,
                    operation: *operation,
                    class: *other,
                    os_code: *os_code,
                });
            }
        }
    }

    /// The measured arena footprint, given the exact number of directories
    /// still queued.
    pub(crate) fn footprint(&self, pending: u64) -> ArenaFootprint {
        let (name_len, name_capacity) = self.tree.name_bytes();
        ArenaFootprint::measure(
            self.tree.len(),
            name_capacity,
            name_len,
            self.tree.directory_count(),
            pending,
        )
    }

    /// Rolls up, validates, and freezes.
    ///
    /// The rollup is [`TreeBuilder::rollup`]: a single **iterative** descending
    /// sweep over the directory index. It is a valid post-order because a child
    /// node can only be created while its parent's listing is being absorbed,
    /// so a child always has a higher id than its parent — which holds no
    /// matter which scheduler discovered the directories, and in which order.
    ///
    /// # Errors
    ///
    /// [`ScanError::Arena`] if the arena fails its structural invariants.
    pub(crate) fn finish(mut self) -> Result<Artifacts, ScanError> {
        self.tree.rollup().map_err(ScanError::Arena)?;
        let root = self.tree.root();
        let tree = self.tree.finish().map_err(ScanError::Arena)?;
        let root_totals = tree.dir_totals(root).copied().unwrap_or(DirTotals::EMPTY);

        let mut counts = self.counts;
        counts.retained_nodes = u64::try_from(tree.len()).unwrap_or(u64::MAX);
        Counters::set(&self.counters.retained_nodes, counts.retained_nodes);
        Counters::set(&self.counters.logical_bytes, root_totals.logical);
        Counters::set(&self.counters.allocated_bytes, root_totals.allocated);

        Ok(Artifacts {
            tree,
            counts,
            totals: ScanTotals {
                logical: root_totals.logical,
                allocated: root_totals.allocated,
            },
            errors: self.errors,
            error_counts: self.error_counts,
            excluded_roots: self.excluded_roots,
            mutations: self.mutations,
        })
    }

    fn push_child(&mut self, parent: NodeId, node: Node) -> Result<NodeId, ScanError> {
        let id = self.tree.push_child(parent, node).map_err(ScanError::Arena)?;
        Counters::add(&self.counters.retained_nodes, 1);
        Ok(id)
    }

    /// Interns a name, downgrading an over-long name to a recorded error rather
    /// than a fatal one: one 70 KiB name must not end a twelve-minute scan.
    fn intern(&mut self, name: &[u8], path: &[u8]) -> Result<Option<NameRef>, ScanError> {
        match self.tree.intern(name) {
            Ok(reference) => Ok(Some(reference)),
            Err(ArenaError::NameTooLong { .. }) => {
                self.record(ScanError::Io {
                    path: DisplayPath::from_bytes(path),
                    operation: Operation::ReadDir,
                    class: ErrorClass::NameTooLong,
                    os_code: 0,
                });
                Ok(None)
            }
            Err(fatal) => Err(ScanError::Arena(fatal)),
        }
    }

    fn mark(&mut self, id: NodeId, bits: u16) {
        if let Some(node) = self.tree.node_mut(id) {
            node.flags |= bits;
        }
    }

    fn note_mutation(&mut self) {
        self.mutations = self.mutations.saturating_add(1);
        Counters::add(&self.counters.mutations, 1);
    }

    fn record(&mut self, error: ScanError) {
        Counters::add(&self.counters.errors, 1);
        let class = error.class();
        let operation = error.operation();
        match self
            .error_counts
            .iter_mut()
            .find(|count| count.class == class && count.operation == operation)
        {
            Some(existing) => existing.count = existing.count.saturating_add(1),
            None => self.error_counts.push(ErrorClassCount {
                class,
                operation,
                count: 1,
            }),
        }
        if self.errors.len() < MAX_DETAILED_ERRORS {
            self.errors.push(error);
        }
    }
}

/// The path below the scan root, with no leading separator.
fn relative_bytes(path: &[u8], root_prefix_len: usize) -> &[u8] {
    let tail = path.get(root_prefix_len..).unwrap_or(b"");
    tail.strip_prefix(b"/").unwrap_or(tail)
}

fn child_display(parent: &Path, name: &SmallName) -> DisplayPath {
    let mut bytes = path_bytes(parent).to_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name.as_bytes());
    DisplayPath::from_bytes(&bytes)
}

/// Turns a [`Path`] into the bytes the arena stores.
pub(crate) fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Builds a [`PathBuf`] from a parent and raw name bytes without going through
/// `String`.
pub(crate) fn join_bytes(parent: &Path, name: &[u8]) -> PathBuf {
    parent.join(OsStr::from_bytes(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_relative_path_drops_the_root_and_its_separator() {
        assert_eq!(relative_bytes(b"/foo", 1), b"foo");
        assert_eq!(relative_bytes(b"/a/b/c", 4), b"c");
        assert_eq!(relative_bytes(b"/a", 2), b"");
        assert_eq!(relative_bytes(b"/a", 99), b"", "an out-of-range prefix is not a panic");
    }

    #[test]
    fn a_child_display_path_escapes_non_utf8_bytes() {
        let shown = child_display(Path::new("/tmp"), &SmallName::from_bytes(&[0xff]));
        assert_eq!(shown.as_str(), "/tmp/%FF");
        let from_root = child_display(Path::new("/"), &SmallName::from_bytes(b"etc"));
        assert_eq!(from_root.as_str(), "/etc", "no doubled separator at the root");
    }

    #[test]
    fn joining_preserves_bytes_that_are_not_utf8() {
        let joined = join_bytes(Path::new("/tmp"), &[0xff, 0xfe]);
        assert_eq!(path_bytes(&joined), &[b'/', b't', b'm', b'p', b'/', 0xff, 0xfe]);
    }
}
