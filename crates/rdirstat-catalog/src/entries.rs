//! `entries`: one row per retained node of a completed scan.
//!
//! # Columns
//!
//! | column        | type    | notes |
//! |---------------|---------|-------|
//! | `entry_id`    | `UInt32`| arena node id within this scan |
//! | `parent_id`   | `UInt32`| null for the scan root |
//! | `path`        | `Utf8`  | absolute; lossy (U+FFFD) only when `path_lossy` |
//! | `path_bytes`  | `Binary`| absolute, lossless filesystem bytes — act on this, never on `path` |
//! | `path_lossy`  | `Boolean` | `path` is not byte-identical to `path_bytes` |
//! | `source_label`| `Utf8`  | |
//! | `kind`        | `Utf8`  | `file`, `dir`, `symlink`, or `other` |
//! | `logical`     | `UInt64`| node's own bytes; zero for directories |
//! | `allocated`   | `UInt64`| node's own allocated bytes |
//! | `mtime_ns`    | `Int64` | `mtime_s * 1e9`, saturating at the `i64` range |
//! | `mtime_s`     | `Int64` | the snapshot's lossless whole seconds |
//! | `dev`         | `UInt64`| the scan's volume `st_dev` |
//! | `ino`         | `UInt64`| always null: `.rdstat` snapshots do not record inodes |
//! | `category`    | `UInt8` | `rdirstat-classify` category index |
//! | `ext`         | `Utf8`  | lowercase extension of a file's name, else null |
//! | `flags`       | `UInt16`| `rdirstat_core::flags` bits |
//!
//! Directory `logical`/`allocated` are the node's own (zero/inode) values,
//! not subtree totals: summing a column over a subtree must not double count.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Int64Builder, StringBuilder, UInt8Builder, UInt16Builder, UInt32Builder,
    UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use rdirstat_core::{CompletedScan, Kind, MAX_TREE_DEPTH, Node, NodeId};

use crate::store::{validate_run_id, write_part};
use crate::{CatalogError, ENTRIES_BATCH_ROWS};

/// Part-file kind for scan entries.
pub(crate) const ENTRIES: &str = "entries";

/// Longest extension kept; anything longer is a name with a dot, not a type.
const MAX_EXT_BYTES: usize = 16;

pub(crate) static ENTRY_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new("entry_id", DataType::UInt32, false),
        Field::new("parent_id", DataType::UInt32, true),
        Field::new("path", DataType::Utf8, false),
        Field::new("path_bytes", DataType::Binary, false),
        Field::new("path_lossy", DataType::Boolean, false),
        Field::new("source_label", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("logical", DataType::UInt64, false),
        Field::new("allocated", DataType::UInt64, false),
        Field::new("mtime_ns", DataType::Int64, false),
        Field::new("mtime_s", DataType::Int64, false),
        Field::new("dev", DataType::UInt64, false),
        Field::new("ino", DataType::UInt64, true),
        Field::new("category", DataType::UInt8, false),
        Field::new("ext", DataType::Utf8, true),
        Field::new("flags", DataType::UInt16, false),
    ]))
});

fn kind_key(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Directory => "dir",
        Kind::Symlink => "symlink",
        _ => "other",
    }
}

/// Lowercase extension of `name`, or `None` for dotfiles, trailing dots,
/// over-long suffixes, and non-UTF-8 suffixes.
pub(crate) fn extension(name: &[u8]) -> Option<String> {
    let dot = name.iter().rposition(|&b| b == b'.')?;
    if dot == 0 {
        return None;
    }
    let ext = name.get(dot + 1..)?;
    if ext.is_empty() || ext.len() > MAX_EXT_BYTES {
        return None;
    }
    std::str::from_utf8(ext).ok().map(str::to_lowercase)
}

/// Column builders for one batch.
struct Batch {
    entry_id: UInt32Builder,
    parent_id: UInt32Builder,
    path: StringBuilder,
    path_bytes: BinaryBuilder,
    path_lossy: BooleanBuilder,
    source_label: StringBuilder,
    kind: StringBuilder,
    logical: UInt64Builder,
    allocated: UInt64Builder,
    mtime_ns: Int64Builder,
    mtime_s: Int64Builder,
    dev: UInt64Builder,
    ino: UInt64Builder,
    category: UInt8Builder,
    ext: StringBuilder,
    flags: UInt16Builder,
    rows: usize,
}

impl Batch {
    fn new() -> Self {
        let n = ENTRIES_BATCH_ROWS;
        Self {
            entry_id: UInt32Builder::with_capacity(n),
            parent_id: UInt32Builder::with_capacity(n),
            path: StringBuilder::with_capacity(n, n * 64),
            path_bytes: BinaryBuilder::with_capacity(n, n * 64),
            path_lossy: BooleanBuilder::with_capacity(n),
            source_label: StringBuilder::with_capacity(n, n * 8),
            kind: StringBuilder::with_capacity(n, n * 4),
            logical: UInt64Builder::with_capacity(n),
            allocated: UInt64Builder::with_capacity(n),
            mtime_ns: Int64Builder::with_capacity(n),
            mtime_s: Int64Builder::with_capacity(n),
            dev: UInt64Builder::with_capacity(n),
            ino: UInt64Builder::with_capacity(n),
            category: UInt8Builder::with_capacity(n),
            ext: StringBuilder::with_capacity(n, n * 4),
            flags: UInt16Builder::with_capacity(n),
            rows: 0,
        }
    }

    #[allow(clippy::too_many_arguments, reason = "one call site, one row")]
    fn push(
        &mut self,
        id: NodeId,
        parent: Option<NodeId>,
        node: &Node,
        path: &[u8],
        name: &[u8],
        label: &str,
        dev: u64,
    ) {
        let kind = node.kind();
        self.entry_id.append_value(id.raw());
        self.parent_id.append_option(parent.map(NodeId::raw));
        if let Ok(text) = std::str::from_utf8(path) {
            self.path.append_value(text);
            self.path_lossy.append_value(false);
        } else {
            self.path.append_value(String::from_utf8_lossy(path));
            self.path_lossy.append_value(true);
        }
        self.path_bytes.append_value(path);
        self.source_label.append_value(label);
        self.kind.append_value(kind_key(kind));
        self.logical.append_value(node.size);
        self.allocated.append_value(node.alloc);
        self.mtime_ns.append_value(node.mtime.saturating_mul(1_000_000_000));
        self.mtime_s.append_value(node.mtime);
        self.dev.append_value(dev);
        self.ino.append_null();
        self.category.append_value(node.category);
        self.ext
            .append_option(if kind == Kind::File { extension(name) } else { None });
        self.flags.append_value(node.flags);
        self.rows += 1;
    }

    fn flush(&mut self, writer: &mut ArrowWriter<std::fs::File>) -> Result<(), CatalogError> {
        if self.rows == 0 {
            return Ok(());
        }
        let columns: Vec<ArrayRef> = vec![
            Arc::new(self.entry_id.finish()),
            Arc::new(self.parent_id.finish()),
            Arc::new(self.path.finish()),
            Arc::new(self.path_bytes.finish()),
            Arc::new(self.path_lossy.finish()),
            Arc::new(self.source_label.finish()),
            Arc::new(self.kind.finish()),
            Arc::new(self.logical.finish()),
            Arc::new(self.allocated.finish()),
            Arc::new(self.mtime_ns.finish()),
            Arc::new(self.mtime_s.finish()),
            Arc::new(self.dev.finish()),
            Arc::new(self.ino.finish()),
            Arc::new(self.category.finish()),
            Arc::new(self.ext.finish()),
            Arc::new(self.flags.finish()),
        ];
        self.rows = 0;
        writer.write(&RecordBatch::try_new(Arc::clone(&ENTRY_SCHEMA), columns)?)?;
        Ok(())
    }
}

#[cfg(unix)]
fn os_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

/// Writes every retained node of `scan` as a new
/// `entries-<run_id>-<seq>.parquet` in `out_dir`.
///
/// Paths are absolute: `scan.root_path` joined with each node's name chain
/// below the root. The root node's own stored name is not used, so a scan
/// whose root was interned as a bare name still yields absolute paths.
/// The walk is depth-first with one reused path buffer, so a directory's path
/// is built once, not once per child.
///
/// # Errors
///
/// [`CatalogError::InvalidRunId`], [`CatalogError::Tree`] if the arena has a
/// dangling id or exceeds [`MAX_TREE_DEPTH`], or an I/O or encoder failure.
pub fn write_entries(
    scan: &CompletedScan,
    source_label: &str,
    out_dir: &Path,
    run_id: &str,
) -> Result<PathBuf, CatalogError> {
    validate_run_id(run_id)?;
    write_part(out_dir, ENTRIES, run_id, &ENTRY_SCHEMA, |writer| {
        walk(scan, source_label, writer)
    })
}

fn walk(scan: &CompletedScan, label: &str, writer: &mut ArrowWriter<std::fs::File>) -> Result<(), CatalogError> {
    let tree = &scan.tree;
    let dev = scan.volume.device;
    let root_bytes = os_bytes(&scan.root_path);

    let mut batch = Batch::new();
    let mut path: Vec<u8> = Vec::with_capacity(1024);
    // prefix[d] = length of the path of the directory currently open at depth d.
    let mut prefix: Vec<usize> = Vec::new();
    let mut stack: Vec<(NodeId, Option<NodeId>, usize)> = vec![(scan.root, None, 0)];

    while let Some((id, parent, depth)) = stack.pop() {
        let node = tree
            .node(id)
            .ok_or_else(|| CatalogError::Tree(format!("node {} is not in the arena", id.raw())))?;
        let name = tree.name_bytes(id).unwrap_or_default();
        let parent_len = depth.checked_sub(1).and_then(|d| prefix.get(d).copied());
        match parent_len {
            None => {
                path.clear();
                path.extend_from_slice(root_bytes);
            }
            Some(len) => {
                path.truncate(len);
                if !path.ends_with(b"/") {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
            }
        }

        batch.push(id, parent, node, &path, name, label, dev);
        if batch.rows >= ENTRIES_BATCH_ROWS {
            batch.flush(writer)?;
        }

        if node.kind().is_directory() {
            if u32::try_from(depth).map_or(true, |d| d >= MAX_TREE_DEPTH) {
                return Err(CatalogError::Tree(format!(
                    "node {} is deeper than {MAX_TREE_DEPTH}",
                    id.raw()
                )));
            }
            prefix.truncate(depth);
            prefix.push(path.len());
            stack.extend(tree.children(id).map(|child| (child, Some(id), depth + 1)));
        }
    }
    batch.flush(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_lowercase_and_skip_dotfiles() {
        assert_eq!(extension(b"Clip.MKV").as_deref(), Some("mkv"));
        assert_eq!(extension(b"a.tar.gz").as_deref(), Some("gz"));
        assert_eq!(extension(b".zshrc"), None);
        assert_eq!(extension(b"trailing."), None);
        assert_eq!(extension(b"noext"), None);
        assert_eq!(extension(b"x.\xff"), None);
        assert_eq!(extension(b"x.abcdefghijklmnopq"), None);
    }
}
