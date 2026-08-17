//! `StdReader` — the correctness oracle.
//!
//! `std::fs::read_dir` plus `DirEntry::metadata`, which Rust documents as
//! equivalent to `symlink_metadata` on Unix, so it does **not** follow
//! symlinks. That costs one metadata syscall per entry and is expected to be
//! the slow baseline; the point is that no faster reader may disagree with it
//! silently (docs/02-SCANNER.md#stdreader--correctness-oracle).

use std::cell::RefCell;
use std::fs::{self, DirEntry, Metadata};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use rdirstat_core::{ErrorClass, Kind, Operation, ScanError};

use crate::cancel::{CANCEL_CHECK_ENTRIES, CancelToken};
use crate::entry::{EntryError, RawEntry, RawEntryBatch, SmallName};
use crate::reader::{DirHandle, DirReader, ReadDirError, classify_os_error};

/// Entries per batch, unless the byte bound trips first.
pub const DEFAULT_BATCH_ENTRIES: usize = 512;

/// Name bytes per batch. Bounding bytes as well as count is what keeps a
/// directory full of 255-byte names from turning one batch into megabytes.
pub const DEFAULT_BATCH_NAME_BYTES: usize = 64 * 1024;

/// Reusable per-thread scratch, so the common path allocates once per thread
/// rather than once per directory.
#[derive(Debug, Default)]
struct Scratch {
    entries: Vec<RawEntry>,
    errors: Vec<EntryError>,
}

thread_local! {
    static SCRATCH: RefCell<Option<Scratch>> = const { RefCell::new(None) };
}

/// Takes the thread-local scratch for the duration of one directory and puts
/// it back on drop, including on an early return.
///
/// Taking rather than borrowing means a re-entrant call (which the schedulers
/// never make) allocates fresh buffers instead of panicking on a double borrow.
#[derive(Debug, Default)]
struct ScratchGuard {
    scratch: Scratch,
}

impl ScratchGuard {
    fn acquire(entries: usize) -> Self {
        let mut scratch = SCRATCH.with(|cell| cell.borrow_mut().take()).unwrap_or_default();
        scratch.entries.clear();
        scratch.errors.clear();
        scratch.entries.reserve(entries);
        Self { scratch }
    }

    fn parts(&mut self) -> (&mut Vec<RawEntry>, &mut Vec<EntryError>) {
        (&mut self.scratch.entries, &mut self.scratch.errors)
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let mut scratch = core::mem::take(&mut self.scratch);
        scratch.entries.clear();
        scratch.errors.clear();
        SCRATCH.with(|cell| {
            *cell.borrow_mut() = Some(scratch);
        });
    }
}

/// The portable, deliberately unclever reader.
#[derive(Clone, Copy, Debug)]
pub struct StdReader {
    batch_entries: usize,
    batch_name_bytes: usize,
    detect_broken_symlinks: bool,
}

impl Default for StdReader {
    fn default() -> Self {
        Self::new()
    }
}

impl StdReader {
    /// A reader with the default batch bounds.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batch_entries: DEFAULT_BATCH_ENTRIES,
            batch_name_bytes: DEFAULT_BATCH_NAME_BYTES,
            detect_broken_symlinks: true,
        }
    }

    /// Overrides the batch bounds. Both are clamped to at least one entry and
    /// one byte so a zero can never disable batching.
    #[must_use]
    pub const fn with_batch_bounds(mut self, entries: usize, name_bytes: usize) -> Self {
        self.batch_entries = if entries == 0 { 1 } else { entries };
        self.batch_name_bytes = if name_bytes == 0 { 1 } else { name_bytes };
        self
    }

    /// Whether to spend one extra `stat` per symlink to set
    /// [`flags::BROKEN_SYMLINK`](rdirstat_core::flags::BROKEN_SYMLINK).
    ///
    /// On by default. It is a per-symlink cost, and symlinks are rare, but on a
    /// stalled network mount the resolution is what blocks.
    #[must_use]
    pub const fn with_broken_symlink_detection(mut self, detect: bool) -> Self {
        self.detect_broken_symlinks = detect;
        self
    }

    /// Rechecks that `dir` is still a directory on the expected device.
    ///
    /// The no-follow `symlink_metadata` plus this recheck is what stops a
    /// directory that was replaced by a symlink between enumeration and descent
    /// from being walked (docs/02-SCANNER.md#errors-and-race-handling).
    fn recheck(dir: &DirHandle) -> Result<(), ReadDirError> {
        let metadata =
            fs::symlink_metadata(dir.path()).map_err(|error| ReadDirError::from_io(Operation::OpenDir, &error))?;
        if !metadata.is_dir() {
            return Err(ReadDirError::Os {
                operation: Operation::OpenDir,
                class: ErrorClass::NotADirectory,
                os_code: 0,
            });
        }
        if metadata.dev() != dir.device() {
            return Err(ReadDirError::Os {
                operation: Operation::OpenDir,
                class: ErrorClass::NotADirectory,
                os_code: 0,
            });
        }
        Ok(())
    }

    fn raw_entry(&self, entry: &DirEntry, metadata: &Metadata) -> RawEntry {
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            Kind::Directory
        } else if file_type.is_file() {
            Kind::File
        } else if file_type.is_symlink() {
            Kind::Symlink
        } else if file_type.is_socket() {
            Kind::Socket
        } else if file_type.is_fifo() {
            Kind::Fifo
        } else if file_type.is_char_device() {
            Kind::CharDevice
        } else if file_type.is_block_device() {
            Kind::BlockDevice
        } else {
            Kind::Unknown
        };

        let mut platform_flags = platform_flags(metadata);
        if kind == Kind::Symlink && self.detect_broken_symlinks && fs::metadata(entry.path()).is_err() {
            platform_flags |= BROKEN_SYMLINK_PROBE;
        }

        RawEntry {
            name: SmallName::from_bytes(entry.file_name().as_bytes()),
            kind,
            size: metadata.size(),
            alloc: metadata.blocks().saturating_mul(512),
            mtime: metadata.mtime(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            mode: metadata.mode(),
            platform_flags,
        }
    }
}

/// Reader-private bit meaning "the symlink target did not resolve".
///
/// It rides in `platform_flags` alongside the Darwin `st_flags` bits, which
/// occupy the low 24 bits, so the two never collide.
pub const BROKEN_SYMLINK_PROBE: u32 = 1 << 31;

#[cfg(target_os = "macos")]
fn platform_flags(metadata: &Metadata) -> u32 {
    std::os::macos::fs::MetadataExt::st_flags(metadata)
}

#[cfg(not(target_os = "macos"))]
fn platform_flags(_metadata: &Metadata) -> u32 {
    0
}

impl DirReader for StdReader {
    fn read_batches(
        &self,
        dir: &DirHandle,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(RawEntryBatch<'_>) -> ControlFlow<ScanError>,
    ) -> Result<(), ReadDirError> {
        if cancel.is_cancelled() {
            return Err(ReadDirError::Cancelled);
        }
        Self::recheck(dir)?;

        let iterator = fs::read_dir(dir.path()).map_err(|error| ReadDirError::from_io(Operation::OpenDir, &error))?;

        let mut guard = ScratchGuard::acquire(self.batch_entries);
        let mut name_bytes = 0_usize;
        let mut since_check = 0_u32;

        for next in iterator {
            since_check += 1;
            if since_check >= CANCEL_CHECK_ENTRIES {
                since_check = 0;
                if cancel.is_cancelled() {
                    return Err(ReadDirError::Cancelled);
                }
            }

            let entry = match next {
                Ok(entry) => entry,
                Err(error) => {
                    // A failure of the iterator itself ends the listing: the
                    // child set is partial and the node is marked as such.
                    return Err(ReadDirError::from_io(Operation::ReadDir, &error));
                }
            };

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    let (_, errors) = guard.parts();
                    errors.push(EntryError {
                        name: SmallName::from_bytes(entry.file_name().as_bytes()),
                        operation: Operation::Metadata,
                        class: classify_os_error(&error),
                        os_code: error.raw_os_error().unwrap_or(0),
                    });
                    continue;
                }
            };

            let raw = self.raw_entry(&entry, &metadata);
            if raw.name.is_dot_entry() {
                continue;
            }
            name_bytes += raw.name.len();
            let (entries, _) = guard.parts();
            entries.push(raw);

            let full = {
                let (entries, _) = guard.parts();
                entries.len() >= self.batch_entries || name_bytes >= self.batch_name_bytes
            };
            if full {
                let (entries, errors) = guard.parts();
                if let ControlFlow::Break(error) = sink(RawEntryBatch::new(entries, errors)) {
                    return Err(ReadDirError::Aborted(error));
                }
                entries.clear();
                errors.clear();
                name_bytes = 0;
            }
        }

        let (entries, errors) = guard.parts();
        let has_tail = !entries.is_empty() || !errors.is_empty();
        if has_tail && let ControlFlow::Break(error) = sink(RawEntryBatch::new(entries, errors)) {
            return Err(ReadDirError::Aborted(error));
        }

        if cancel.is_cancelled() {
            return Err(ReadDirError::Cancelled);
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "std"
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;

    use rdirstat_core::NodeId;

    use super::*;

    fn collect(reader: &StdReader, dir: &DirHandle) -> Result<Vec<RawEntry>, ReadDirError> {
        let mut out = Vec::new();
        let cancel = CancelToken::new();
        reader.read_batches(dir, &cancel, &mut |batch| {
            out.extend_from_slice(batch.entries());
            ControlFlow::Continue(())
        })?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn handle(path: &std::path::Path) -> DirHandle {
        let device = fs::symlink_metadata(path).expect("root exists").dev();
        DirHandle::new(path.to_path_buf(), NodeId::ROOT, device, 0)
    }

    #[test]
    fn a_symlink_is_reported_as_a_leaf_and_never_resolved() {
        let temp = tempfile::tempdir().expect("temp dir");
        let target = temp.path().join("target");
        fs::create_dir(&target).expect("target dir");
        File::create(target.join("hidden")).expect("file inside the target");
        std::os::unix::fs::symlink(&target, temp.path().join("link")).expect("symlink");

        let reader = StdReader::new();
        let entries = collect(&reader, &handle(temp.path())).expect("readable");
        let names: Vec<&[u8]> = entries.iter().map(|entry| entry.name.as_bytes()).collect();
        assert_eq!(names, vec![b"link".as_slice(), b"target".as_slice()]);
        let link = entries.iter().find(|e| e.name.as_bytes() == b"link").expect("link");
        assert_eq!(link.kind, Kind::Symlink, "a symlink to a directory is still a symlink");
    }

    #[test]
    fn a_broken_symlink_is_flagged_without_being_followed() {
        let temp = tempfile::tempdir().expect("temp dir");
        std::os::unix::fs::symlink(temp.path().join("nowhere"), temp.path().join("dangling")).expect("symlink");
        let entries = collect(&StdReader::new(), &handle(temp.path())).expect("readable");
        let dangling = entries.first().expect("one entry");
        assert_eq!(dangling.kind, Kind::Symlink);
        assert_ne!(dangling.platform_flags & BROKEN_SYMLINK_PROBE, 0);
    }

    #[test]
    fn batching_is_bounded_but_loses_nothing() {
        let temp = tempfile::tempdir().expect("temp dir");
        for index in 0..37 {
            let mut file = File::create(temp.path().join(format!("f{index:03}"))).expect("file");
            file.write_all(b"x").expect("write");
        }
        let reader = StdReader::new().with_batch_bounds(4, usize::MAX);
        let mut batches = 0_usize;
        let mut total = 0_usize;
        let cancel = CancelToken::new();
        reader
            .read_batches(&handle(temp.path()), &cancel, &mut |batch| {
                batches += 1;
                total += batch.len();
                assert!(batch.len() <= 4, "batch bound honoured");
                ControlFlow::Continue(())
            })
            .expect("readable");
        assert_eq!(total, 37);
        assert_eq!(batches, 10, "37 entries in batches of 4");
    }

    #[test]
    fn metadata_reports_logical_and_allocated_separately() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("sparse");
        let file = File::create(&path).expect("file");
        file.set_len(4 * 1024 * 1024).expect("sparse extension");
        drop(file);

        let entries = collect(&StdReader::new(), &handle(temp.path())).expect("readable");
        let sparse = entries.first().expect("one entry");
        assert_eq!(sparse.size, 4 * 1024 * 1024);
        assert!(
            sparse.alloc <= sparse.size,
            "allocated {} should not exceed logical {}",
            sparse.alloc,
            sparse.size
        );
    }

    #[test]
    fn a_replaced_directory_is_not_walked() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("d");
        fs::create_dir(&path).expect("dir");
        let device = fs::symlink_metadata(&path).expect("stat").dev();
        let stale = DirHandle::new(path.clone(), NodeId::ROOT, device.wrapping_add(1), 1);
        let error = collect(&StdReader::new(), &stale).expect_err("device recheck fails");
        assert_eq!(error.class(), Some(ErrorClass::NotADirectory));
    }

    #[test]
    fn cancellation_stops_the_read() {
        let temp = tempfile::tempdir().expect("temp dir");
        File::create(temp.path().join("a")).expect("file");
        let cancel = CancelToken::new();
        cancel.cancel();
        let error = StdReader::new()
            .read_batches(&handle(temp.path()), &cancel, &mut |_| ControlFlow::Continue(()))
            .expect_err("cancelled");
        assert!(matches!(error, ReadDirError::Cancelled));
    }

    #[test]
    fn a_missing_directory_is_a_classified_error_not_a_panic() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing = temp.path().join("gone");
        let stale = DirHandle::new(missing, NodeId::ROOT, 0, 1);
        let error = collect(&StdReader::new(), &stale).expect_err("missing");
        assert_eq!(error.class(), Some(ErrorClass::NotFound));
    }
}
