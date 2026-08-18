//! Digests for the details panel.
//!
//! Two questions get asked of a selected node, and they are not the same
//! question, so they do not get the same answer.
//!
//! **A file has contents, so it gets SHA-256 of those contents.** That is the
//! digest everything else in the world means by "the hash of this file": it
//! matches `shasum -a 256`, it identifies the bytes independently of where they
//! live, and it is what a hash set or a reputation lookup can be keyed on.
//! Reading a file costs what the file costs, so this never runs on its own —
//! [`contents`] is only reached from an explicit request.
//!
//! **A directory has no contents of its own**, and hashing everything beneath
//! it would mean reading the whole subtree: on the volume this was written for,
//! 1.73 TB across 24.7M entries. So a directory gets a *structure* digest
//! instead — a fold over the shape of the subtree as the scan already recorded
//! it: names, sizes, mtimes, and nesting. It reads no file data at all, answers
//! from the arena already in memory, and returns while the panel is still
//! opening.
//!
//! The two MUST NOT be confused, so they are separated three ways: different
//! functions, different commands, and a domain-separation prefix mixed into the
//! structure digest before anything else. That last one means a structure
//! digest can never collide with a content SHA-256 even if some caller
//! contrived a file whose bytes were exactly the canonical encoding below. A
//! metadata hash passing for a content hash is the failure worth engineering
//! against here: it would claim two directories hold identical *data* when all
//! that was compared was their listings.

use std::fs::File;
use std::io::{BufReader, Read};

use rdirstat_core::{CompletedScan, NodeId, Tree};
use sha2::{Digest, Sha256};

use crate::actions;

/// Read size for the streaming content hash. Matches `verify.rs`, which hashes
/// for the Dupes report — no reason for the two to disagree.
const READ_CHUNK: usize = 64 * 1024;

/// Mixed in before a structure digest so it cannot equal a content digest.
///
/// Versioned: if the canonical encoding below ever changes, this string changes
/// with it, and old digests stop comparing equal to new ones instead of
/// silently disagreeing about what they mean.
const STRUCTURE_DOMAIN: &[u8] = b"rdirstat/structure-digest/v1\x00";

/// A structure digest over a subtree, with what it covered.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StructureDigest {
    /// Lowercase hex SHA-256 over the canonical encoding.
    pub(crate) digest: String,
    /// How many arena entries were folded in, so the UI can say what it covered
    /// rather than presenting a bare number with no scope.
    pub(crate) entries: u64,
    /// True when the walk hit its backstop and stopped early. A digest over a
    /// truncated walk is not comparable to one over a whole tree, so the UI has
    /// to be able to say so rather than show it as if it were complete.
    pub(crate) truncated: bool,
}

/// A content digest over one file.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContentDigest {
    /// Lowercase hex SHA-256 of the file's bytes.
    pub(crate) digest: String,
    /// Bytes actually read. Compared against the scan's recorded size by the
    /// caller, this is how a file that changed under the scan shows up.
    pub(crate) bytes: u64,
}

/// Folds the shape of `node`'s subtree into one digest.
///
/// Returns `None` when `node` is not in this tree.
///
/// # Determinism
///
/// Children are sorted by name before descending. This is the whole reason the
/// digest is worth anything: `readdir` order is not stable across filesystems
/// or even across two scans of the same directory, so folding in arena order
/// would produce a different digest every scan and answer "did this change?"
/// with "yes" every single time.
pub(crate) fn structure(tree: &Tree, node: NodeId) -> Option<StructureDigest> {
    // A virtual `<Files>` group has no arena node of its own; digest its owner.
    let start = node.group_owner().unwrap_or(node);
    tree.node(start)?;

    let mut hasher = Sha256::new();
    hasher.update(STRUCTURE_DOMAIN);

    // Depth travels with the id: without it the encoding is a flat stream of
    // records, and two differently-nested trees holding the same entries would
    // fold to the same digest.
    let mut stack = vec![(start, 0_u32)];
    let mut entries = 0_u64;

    // The arena is finite and acyclic by `Tree`'s own freeze-time validation,
    // so this is a backstop against a tree that escaped it, not an expected
    // limit — but if it ever fires the digest is partial and says so.
    let mut budget = tree.len().saturating_mul(2).saturating_add(16);
    let mut truncated = false;

    while let Some((id, depth)) = stack.pop() {
        if budget == 0 {
            truncated = true;
            break;
        }
        budget -= 1;

        let Some(entry) = tree.node(id) else { continue };
        let name = tree.name_bytes(id).unwrap_or(b"");

        // Length-prefixed, so a name containing the separator cannot be made to
        // impersonate a different (name, size) pair.
        hasher.update(depth.to_le_bytes());
        hasher.update([u8::from(entry.kind().is_file())]);
        hasher.update(entry.size.to_le_bytes());
        hasher.update(entry.mtime.to_le_bytes());
        hasher.update(u32::try_from(name.len()).unwrap_or(u32::MAX).to_le_bytes());
        hasher.update(name);
        entries = entries.saturating_add(1);

        let mut children: Vec<NodeId> = tree.children(id).collect();
        children.sort_by(|a, b| tree.name_bytes(*a).cmp(&tree.name_bytes(*b)));
        // Pushed in reverse so the LIFO stack POPS them in sorted order. Push
        // them forwards and the walk runs backwards, which is still
        // deterministic but no longer matches the order this documents.
        let next_depth = depth.saturating_add(1);
        for child in children.into_iter().rev() {
            stack.push((child, next_depth));
        }
    }

    Some(StructureDigest {
        digest: format!("{:x}", hasher.finalize()),
        entries,
        truncated,
    })
}

/// SHA-256 over the whole of one file.
///
/// Streams in [`READ_CHUNK`] blocks rather than reading the file in: this is
/// reached for files of any size, and a details panel must not need the file to
/// fit in memory to describe it.
///
/// # Errors
///
/// The node's path cannot be resolved, or the file cannot be opened or read.
pub(crate) fn contents(scan: &CompletedScan, node: NodeId) -> Result<ContentDigest, String> {
    let (path, _) = actions::resolve(scan, node).map_err(|error| error.to_string())?;

    let file = File::open(&path).map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; READ_CHUNK];
    let mut bytes = 0_u64;

    loop {
        let got = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if got == 0 {
            break;
        }
        hasher.update(&buffer[..got]);
        bytes = bytes.saturating_add(u64::try_from(got).unwrap_or(0));
    }

    Ok(ContentDigest {
        digest: format!("{:x}", hasher.finalize()),
        bytes,
    })
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
    fn a_file_digest_is_the_sha256_the_rest_of_the_world_agrees_on() {
        // The point of using SHA-256 rather than something cheaper: the value
        // has to match `shasum -a 256`, or it cannot be pasted anywhere useful.
        // This is the published digest of the three bytes "abc".
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("abc.bin"), b"abc").expect("write");
        let scan = scan_of(dir.path());

        let hashed = contents(&scan, node_named(&scan, "abc.bin")).expect("digest");

        assert_eq!(
            hashed.digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hashed.bytes, 3);
    }

    #[test]
    fn a_file_larger_than_one_read_chunk_still_hashes_whole() {
        // Guards the streaming loop: an early version that read one buffer and
        // stopped would pass every small fixture and silently hash a prefix of
        // every real file.
        let dir = tempfile::tempdir().expect("tempdir");
        let size = READ_CHUNK * 3 + 17;
        std::fs::write(dir.path().join("big.bin"), vec![b'z'; size]).expect("write");
        let scan = scan_of(dir.path());

        let hashed = contents(&scan, node_named(&scan, "big.bin")).expect("digest");

        assert_eq!(hashed.bytes, u64::try_from(size).expect("fits"));
    }

    #[test]
    fn a_structure_digest_is_stable_when_nothing_changed() {
        // Two folds over the same tree must agree, or the digest cannot answer
        // "did this change?" at all.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("nested")).expect("mkdir");
        std::fs::write(dir.path().join("nested/a.txt"), b"aaaa").expect("write");
        std::fs::write(dir.path().join("b.txt"), b"bb").expect("write");
        let scan = scan_of(dir.path());
        let root = scan.tree.root();

        let first = structure(&scan.tree, root).expect("digest");
        let second = structure(&scan.tree, root).expect("digest");

        assert_eq!(first.digest, second.digest);
        assert!(!first.truncated);
        // Root, `nested`, `nested/a.txt`, `b.txt`.
        assert_eq!(first.entries, 4);
    }

    #[test]
    fn a_structure_digest_notices_a_size_change_without_reading_the_bytes() {
        // The useful property: it never opens a file, but a file that grew
        // still moves the digest, because the size came from the scan.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), b"aaaa").expect("write");
        let before = {
            let scan = scan_of(dir.path());
            structure(&scan.tree, scan.tree.root()).expect("digest").digest
        };

        std::fs::write(dir.path().join("a.txt"), b"aaaaaaaa").expect("rewrite");
        let after = {
            let scan = scan_of(dir.path());
            structure(&scan.tree, scan.tree.root()).expect("digest").digest
        };

        assert_ne!(before, after);
    }

    #[test]
    fn a_structure_digest_is_never_a_content_digest() {
        // Domain separation. These answer different questions about the same
        // node and must not be interchangeable, however the encoding evolves.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), b"abc").expect("write");
        let scan = scan_of(dir.path());
        let node = node_named(&scan, "a.txt");

        let shape = structure(&scan.tree, node).expect("digest");
        let bytes = contents(&scan, node).expect("digest");

        assert_ne!(shape.digest, bytes.digest);
    }
}
