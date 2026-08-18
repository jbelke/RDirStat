//! Confirming that candidate duplicates really are the same bytes.
//!
//! [`dupes`](rdirstat_core::dupes) groups files by size, which is cheap and
//! wrong on its own: two files of the same length are a *candidate* pair and
//! nothing more. The panel says so, and until something reads the bytes it can
//! only ever say so. This is the part that reads the bytes.
//!
//! **SHA-256, not the MD5 already in the tree.** The consequence of this report
//! is someone deleting one file of a pair. MD5 collisions are constructible, so
//! "two different files that hash alike" is not a theoretical worry here — it
//! is the exact failure that loses data. The cost difference is irrelevant
//! beside that, and both come from the same `digest` traits.
//!
//! **Two passes, because most candidates are not duplicates.** Files that share
//! a size usually differ early — different videos, different disk images, two
//! unrelated 4 MiB archives. Hashing a whole 58 GB pair to discover that the
//! first kilobyte differs is minutes wasted per pair. So each file is first
//! hashed over its leading [`HEAD_BYTES`], candidates are regrouped on that,
//! and only groups that still have two or more members are read in full. A
//! file shorter than `HEAD_BYTES` is complete after the first pass and is not
//! read twice.
//!
//! **Every file is re-validated before it is opened.** `actions::resolve`
//! checks the recorded dev/ino/kind and that the path is still inside the scan
//! root, so a file replaced since the scan is reported as changed rather than
//! silently hashed and offered for deletion.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};

use rdirstat_core::{ActionError, CompletedScan, DisplayPath, NodeId};
use sha2::{Digest, Sha256};

use crate::actions;

/// How much of each file the first pass reads.
///
/// Large enough that headers, magic numbers and container metadata are all
/// inside it — the places files that share a size actually diverge — and small
/// enough that a pass over a hundred candidates is a handful of megabytes.
const HEAD_BYTES: u64 = 64 * 1024;

/// Read buffer. Matches the head window so the first pass is one fill.
const READ_CHUNK: usize = 64 * 1024;

/// Files confirmed identical by content.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub(crate) struct VerifiedGroup {
    /// Lowercase hex SHA-256 of the whole file.
    pub digest: String,
    /// Every node whose contents hashed to `digest`, in the order asked for.
    pub nodes: Vec<NodeId>,
    /// The size of one member, which they all share.
    pub bytes: u64,
}

/// A candidate that could not be hashed, and why.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub(crate) struct VerifyFailure {
    pub node: NodeId,
    pub path: DisplayPath,
    pub reason: String,
}

/// The result of reading a set of candidates.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub(crate) struct VerifyReport {
    /// Groups of two or more files with identical contents.
    pub groups: Vec<VerifiedGroup>,
    /// Candidates that turned out to be unique. Named rather than dropped: a
    /// file that silently disappears from the list looks like a bug, and "this
    /// one is not a duplicate after all" is the answer the user asked for.
    pub unique: Vec<NodeId>,
    /// Candidates that could not be read, changed since the scan, or left the
    /// scan root.
    pub failed: Vec<VerifyFailure>,
    /// Bytes actually read, both passes summed. Reported so the cost of the
    /// check is visible rather than guessed at.
    pub bytes_read: u64,
}

/// Hashes `nodes` and groups them by content.
pub(crate) fn verify(scan: &CompletedScan, nodes: &[NodeId]) -> VerifyReport {
    let mut bytes_read = 0_u64;
    let mut failed: Vec<VerifyFailure> = Vec::new();

    // --- pass one: the leading window ---
    let mut by_head: HashMap<(String, u64), Vec<NodeId>> = HashMap::new();
    let mut sizes: HashMap<NodeId, u64> = HashMap::new();
    let mut complete: HashMap<NodeId, String> = HashMap::new();

    for &node in nodes {
        match hash_prefix(scan, node, HEAD_BYTES) {
            Ok(head) => {
                bytes_read += head.read;
                sizes.insert(node, head.size);
                if head.whole_file {
                    // Shorter than the window: the first pass already read all
                    // of it, so the head digest IS the full digest.
                    complete.insert(node, head.digest.clone());
                }
                by_head.entry((head.digest, head.size)).or_default().push(node);
            }
            Err(failure) => failed.push(failure),
        }
    }

    // --- pass two: only where a head collision survived ---
    let mut by_full: HashMap<(String, u64), Vec<NodeId>> = HashMap::new();
    let mut unique: Vec<NodeId> = Vec::new();

    for ((_, size), members) in by_head {
        if members.len() < 2 {
            unique.extend(members);
            continue;
        }
        for node in members {
            if let Some(digest) = complete.get(&node) {
                by_full.entry((digest.clone(), size)).or_default().push(node);
                continue;
            }
            match hash_prefix(scan, node, u64::MAX) {
                Ok(full) => {
                    bytes_read += full.read;
                    by_full.entry((full.digest, size)).or_default().push(node);
                }
                Err(failure) => failed.push(failure),
            }
        }
    }

    let mut groups: Vec<VerifiedGroup> = Vec::new();
    for ((digest, bytes), mut members) in by_full {
        if members.len() < 2 {
            unique.extend(members);
            continue;
        }
        // Stable output: the same set of files must produce the same report
        // twice, and a HashMap's order does not.
        members.sort_unstable();
        groups.push(VerifiedGroup {
            digest,
            nodes: members,
            bytes,
        });
    }

    // Biggest recovery first — the reason anyone opened this panel.
    groups.sort_by(|a, b| {
        let a_recovers = a.bytes.saturating_mul(a.nodes.len() as u64 - 1);
        let b_recovers = b.bytes.saturating_mul(b.nodes.len() as u64 - 1);
        b_recovers.cmp(&a_recovers).then_with(|| a.digest.cmp(&b.digest))
    });
    unique.sort_unstable();

    VerifyReport {
        groups,
        unique,
        failed,
        bytes_read,
    }
}

struct Hashed {
    digest: String,
    /// The file's full length, from the same `stat` the read used.
    size: u64,
    /// Bytes actually read.
    read: u64,
    /// The limit was never reached, so this digest covers the whole file.
    whole_file: bool,
}

fn hash_prefix(scan: &CompletedScan, node: NodeId, limit: u64) -> Result<Hashed, VerifyFailure> {
    let (path, _) = actions::resolve(scan, node).map_err(|error| failure(node, &error))?;
    let display = DisplayPath::from_bytes(path.as_os_str().as_encoded_bytes());

    let file = File::open(&path).map_err(|error| VerifyFailure {
        node,
        path: display.clone(),
        reason: error.to_string(),
    })?;
    let size = file
        .metadata()
        .map_err(|error| VerifyFailure {
            node,
            path: display.clone(),
            reason: error.to_string(),
        })?
        .len();

    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; READ_CHUNK];
    let mut read = 0_u64;

    while read < limit {
        let want = usize::try_from((limit - read).min(READ_CHUNK as u64)).unwrap_or(READ_CHUNK);
        let got = reader.read(&mut buffer[..want]).map_err(|error| VerifyFailure {
            node,
            path: display.clone(),
            reason: error.to_string(),
        })?;
        if got == 0 {
            break;
        }
        hasher.update(&buffer[..got]);
        read += got as u64;
    }

    Ok(Hashed {
        digest: format!("{:x}", hasher.finalize()),
        size,
        read,
        // `read < limit` means the reader hit EOF before the cap, so everything
        // there was to read is in the digest.
        whole_file: read < limit || read >= size,
    })
}

fn failure(node: NodeId, error: &ActionError) -> VerifyFailure {
    VerifyFailure {
        node,
        path: DisplayPath::from_bytes(b""),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rdirstat_core::{ScanId, ScanOptions, TreeGeneration};

    use super::*;
    use crate::engine::{ScanOutcome, ScanRequest};
    use crate::progress::ProgressCounters;
    use crate::state::CancelToken;

    fn scan_of(root: &Path) -> Arc<CompletedScan> {
        match crate::engine::run(ScanRequest {
            root: root.to_path_buf(),
            options: ScanOptions::default(),
            scan_id: ScanId::FIRST,
            generation: TreeGeneration::FIRST,
            cancel: Arc::new(CancelToken::new()),
            counters: Arc::new(ProgressCounters::new()),
        }) {
            ScanOutcome::Completed(scan) => Arc::from(scan),
            other => panic!("expected a completed scan, got {other:?}"),
        }
    }

    fn node_named(scan: &CompletedScan, name: &str) -> NodeId {
        scan.tree
            .children(scan.tree.root())
            .find(|node| scan.tree.name_bytes(*node) == Some(name.as_bytes()))
            .unwrap_or_else(|| panic!("no child named {name}"))
    }

    #[test]
    fn same_size_different_bytes_is_not_a_duplicate() {
        // The whole reason this module exists: size grouping calls these a
        // candidate pair, and they are not the same file.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.bin"), vec![b'a'; 4096]).expect("write");
        std::fs::write(dir.path().join("b.bin"), vec![b'b'; 4096]).expect("write");
        let scan = scan_of(dir.path());
        let nodes = vec![node_named(&scan, "a.bin"), node_named(&scan, "b.bin")];

        let report = verify(&scan, &nodes);
        assert!(report.groups.is_empty(), "{report:?}");
        assert_eq!(report.unique.len(), 2, "{report:?}");
        assert!(report.failed.is_empty(), "{report:?}");
    }

    #[test]
    fn identical_bytes_group_under_one_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["one.bin", "two.bin", "three.bin"] {
            std::fs::write(dir.path().join(name), vec![b'q'; 8192]).expect("write");
        }
        let scan = scan_of(dir.path());
        let nodes = vec![
            node_named(&scan, "one.bin"),
            node_named(&scan, "two.bin"),
            node_named(&scan, "three.bin"),
        ];

        let report = verify(&scan, &nodes);
        assert_eq!(report.groups.len(), 1, "{report:?}");
        let group = &report.groups[0];
        assert_eq!(group.nodes.len(), 3);
        assert_eq!(group.bytes, 8192);
        // A real SHA-256, checked against the known digest of 8192 'q' bytes
        // rather than against whatever this code happens to produce.
        let expected = {
            let mut hasher = Sha256::new();
            hasher.update(vec![b'q'; 8192]);
            format!("{:x}", hasher.finalize())
        };
        assert_eq!(group.digest, expected);
        assert_eq!(group.digest.len(), 64, "hex SHA-256 is 64 characters");
    }

    #[test]
    fn files_that_differ_only_past_the_head_window_are_still_told_apart() {
        // The two-pass shortcut's one real risk: identical prefixes. If the
        // second pass were skipped these would be reported as duplicates and
        // one of them offered for deletion.
        let dir = tempfile::tempdir().expect("tempdir");
        let size = usize::try_from(HEAD_BYTES).expect("fits") + 4096;
        let mut a = vec![b'p'; size];
        let mut b = vec![b'p'; size];
        a[size - 1] = b'A';
        b[size - 1] = b'B';
        std::fs::write(dir.path().join("a.bin"), &a).expect("write");
        std::fs::write(dir.path().join("b.bin"), &b).expect("write");
        let scan = scan_of(dir.path());
        let nodes = vec![node_named(&scan, "a.bin"), node_named(&scan, "b.bin")];

        let report = verify(&scan, &nodes);
        assert!(report.groups.is_empty(), "differ in the last byte: {report:?}");
        assert_eq!(report.unique.len(), 2, "{report:?}");
    }

    #[test]
    fn a_short_file_is_not_read_twice() {
        // Both files are well under the head window, so the first pass already
        // hashed all of them; the second pass must reuse that digest rather
        // than re-open and re-read.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.bin"), vec![b'm'; 512]).expect("write");
        std::fs::write(dir.path().join("y.bin"), vec![b'm'; 512]).expect("write");
        let scan = scan_of(dir.path());
        let nodes = vec![node_named(&scan, "x.bin"), node_named(&scan, "y.bin")];

        let report = verify(&scan, &nodes);
        assert_eq!(report.groups.len(), 1, "{report:?}");
        assert_eq!(report.bytes_read, 1024, "512 bytes each, read once: {report:?}");
    }

    #[test]
    fn a_file_deleted_since_the_scan_is_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("gone.bin"), vec![b'g'; 100]).expect("write");
        std::fs::write(dir.path().join("stays.bin"), vec![b'g'; 100]).expect("write");
        let scan = scan_of(dir.path());
        let nodes = vec![node_named(&scan, "gone.bin"), node_named(&scan, "stays.bin")];
        std::fs::remove_file(dir.path().join("gone.bin")).expect("remove");

        let report = verify(&scan, &nodes);
        assert_eq!(report.failed.len(), 1, "{report:?}");
        // And the survivor is not silently promoted to a duplicate of nothing.
        assert!(report.groups.is_empty(), "{report:?}");
    }

    #[test]
    fn the_same_input_produces_the_same_report_twice() {
        // Grouping runs through HashMaps; without the explicit sorts the order
        // would vary per run and a UI diffing two reports would show churn.
        let dir = tempfile::tempdir().expect("tempdir");
        for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
            std::fs::write(dir.path().join(name), vec![b'r'; 2048]).expect("write");
        }
        std::fs::write(dir.path().join("odd.bin"), vec![b's'; 2048]).expect("write");
        let scan = scan_of(dir.path());
        let nodes: Vec<NodeId> = ["a.bin", "b.bin", "c.bin", "d.bin", "odd.bin"]
            .iter()
            .map(|name| node_named(&scan, name))
            .collect();

        assert_eq!(verify(&scan, &nodes), verify(&scan, &nodes));
    }
}
