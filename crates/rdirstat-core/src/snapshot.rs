//! The `*.rdstat` container: one immutable completed scan, encoded.
//!
//! A full-volume scan costs minutes (measured: 8.9M entries, 1.2M directories,
//! 869 GB in 3:11, still with 160K directories pending). Re-walking the volume
//! to look at a tree that has not changed is the single largest tax on both
//! the user and on iterating the app itself, so a completed arena is written
//! once and rehydrated in the time it takes to read the bytes back.
//!
//! ## What this module is, and is not
//!
//! This is the **format**, not the store. It encodes to a [`Write`] and decodes
//! from a [`Read`], and it never names a path, opens a file, or chooses a
//! directory — `rdirstat-core` knows nothing about the filesystem
//! (docs/01-ARCHITECTURE.md). Where snapshots live, how they are named, and how
//! a write is made atomic belong to the shell.
//!
//! Authority is narrow, per docs/01-ARCHITECTURE.md#persistence: **the snapshot
//! restores interaction, the Parquet catalog answers aggregate and cross-scan
//! questions.** A snapshot is not a query engine and is not history; it is the
//! arena, exactly as it was frozen.
//!
//! ## The layout
//!
//! ```text
//! header      56 bytes, little-endian, fixed
//! meta        `meta_len` bytes of JSON — every scalar of the CompletedScan
//! nodes       `node_count` × 48 bytes
//! names       `name_len` bytes, the name blob verbatim
//! dir ids     `dir_count` × 4 bytes
//! dir totals  `dir_count` × 56 bytes
//! trailer     8 bytes, checksum over everything above
//! ```
//!
//! Header fields, at their byte offsets:
//!
//! ```text
//!  0   8  magic            MAGIC
//!  8   4  format_version   FORMAT_VERSION
//! 12   4  endian_marker    ENDIAN_MARKER, so a byte-swapped file fails loudly
//! 16   1  word_size        size_of::<usize>() on the writer, informational
//! 17   1  node_size        48 — catches a Node layout change across versions
//! 18   1  totals_size      56 — likewise for DirTotals
//! 19   1  compression      0 = none; see "Compression" below
//! 20   4  reserved         zero
//! 24   8  meta_len
//! 32   8  node_count
//! 40   8  name_len
//! 48   8  dir_count
//! ```
//!
//! Fixed-width little-endian fields are written explicitly rather than by
//! reinterpreting `Node` as bytes. That costs a pass over the array and buys
//! three things a memory-mapped struct array cannot have in a crate that
//! forbids `unsafe`: the encoding does not depend on the host ABI, a padding
//! byte cannot leak, and `node_size`/`totals_size` turn a layout change into a
//! clean rejection instead of a plausible-looking wrong tree.
//!
//! ## Compression
//!
//! The `compression` byte exists and is written as `NO_COMPRESSION`. Sections
//! *may* be compressed (docs/01-ARCHITECTURE.md#persistence says "may"), and
//! v1 does not, deliberately: on local `NVMe` an uncompressed section is read at
//! device speed with no decompress pass, and the whole point of the file is to
//! come back faster than a rescan. A reader that meets an unknown compression
//! byte rejects the file rather than guessing, so adding zstd later is a
//! version-compatible change to the writer alone.
//!
//! ## Trust
//!
//! A snapshot is **untrusted input**. It may have been truncated by a full
//! disk, corrupted in place, or written by a different build. Nothing decoded
//! here reaches a command until:
//!
//! 1. the header validates — magic, version, endianness, struct sizes;
//! 2. every declared length passes [`Limits`] *before* anything is allocated,
//!    so a corrupt `node_count` cannot become a multi-terabyte reservation;
//! 3. the checksum over the whole file matches;
//! 4. [`Tree::from_parts`] accepts the arena, which re-checks every name range,
//!    every link, the directory index ordering, and the parent-chain depth.
//!
//! Step 4 is what makes the rest safe to be cheap: the structural invariants
//! are the same ones a freshly built tree must satisfy, so a snapshot cannot
//! introduce a tree shape the query paths have never seen.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::dirs::{DirIndex, DirTotals};
use crate::error::{ErrorClassCount, ScanError};
use crate::id::{NodeId, ScanId, TreeGeneration};
use crate::name::{NameBlob, NameRef};
use crate::node::Node;
use crate::scan::{CompletedScan, ConfigHash, ScanCounts, ScanOptions, ScanTotals, VolumeId};
use crate::tree::Tree;
use crate::wire::DisplayPath;

/// Container magic. The trailing `\x1a\n` is the DOS end-of-file plus a newline
/// that `cat`, `less`, and an accidental text-mode transfer all disturb, so a
/// file mangled in transit fails on the magic rather than deeper in.
pub const MAGIC: [u8; 8] = *b"RDSTAT\x1a\n";

/// Container format version. Bumped when the byte layout changes in a way an
/// older reader cannot interpret; a reader refuses any version it does not
/// know rather than reading a prefix it happens to understand.
pub const FORMAT_VERSION: u32 = 1;

/// Written little-endian, so a big-endian or byte-swapped file is rejected on
/// the header instead of producing absurd sizes.
pub const ENDIAN_MARKER: u32 = 0x0102_0304;

/// The only compression this version writes or accepts.
pub const NO_COMPRESSION: u8 = 0;

/// Bytes per encoded [`Node`]. Matches `size_of::<Node>()`, asserted below.
pub const NODE_BYTES: usize = 48;

/// Bytes per encoded [`DirTotals`]. Matches `size_of::<DirTotals>()`.
pub const TOTALS_BYTES: usize = 56;

/// Bytes per encoded [`NodeId`].
const NODE_ID_BYTES: usize = 4;

/// Fixed header length.
const HEADER_BYTES: usize = 56;

/// Trailer length: the checksum.
const TRAILER_BYTES: usize = 8;

/// How many array elements the writer encodes per buffered block.
///
/// The write path must not allocate a second copy of the arena: at the design
/// profile the node array alone is 3.08 GiB, so encoding streams through a
/// small reusable buffer instead of building one contiguous image of the file.
const BLOCK_NODES: usize = 8192;

// The encoded widths must track the in-memory structs, or a snapshot written by
// this build would be misread by it. `node_size`/`totals_size` in the header
// catch a *cross-version* change; these catch a same-version mistake at compile
// time.
const _: () = assert!(NODE_BYTES == core::mem::size_of::<Node>());
const _: () = assert!(TOTALS_BYTES == core::mem::size_of::<DirTotals>());
const _: () = assert!(NODE_ID_BYTES == core::mem::size_of::<NodeId>());

/// How many entries a reader will allocate for before refusing the file.
///
/// These are a defence against a corrupt length field, not a policy about how
/// large a real scan may be. The defaults are the 69M-entry design profile
/// (docs/01-ARCHITECTURE.md#memory-budget) with headroom, so a legitimate
/// snapshot of the largest tree the scanner supports always loads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum retained nodes.
    pub max_nodes: u64,
    /// Maximum directories.
    pub max_dirs: u64,
    /// Maximum name-blob bytes.
    pub max_name_bytes: u64,
    /// Maximum metadata bytes. The metadata is scalars plus a bounded error
    /// list, so this is small on purpose: a large value here is corruption.
    pub max_meta_bytes: u64,
}

impl Limits {
    /// Ceilings matching the documented design profile.
    pub const DESIGN_PROFILE: Self = Self {
        max_nodes: 128 << 20,
        max_dirs: 32 << 20,
        max_name_bytes: 8 << 30,
        max_meta_bytes: 16 << 20,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DESIGN_PROFILE
    }
}

/// Why a snapshot could not be written or read.
///
/// Every read failure names what did not match, because the caller's next move
/// differs: a version mismatch means "rescan", a checksum mismatch means "this
/// file is damaged", and a limit breach means "this file is not plausible".
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The underlying reader or writer failed.
    #[error("snapshot I/O failed: {0}")]
    Io(#[from] std::io::Error),

    /// The file does not start with [`MAGIC`].
    #[error("not an rdstat snapshot: wrong magic")]
    BadMagic,

    /// The file ended before a declared section did.
    #[error("snapshot truncated: wanted {wanted} bytes for {section}, got {got}")]
    Truncated {
        /// Which section ran out.
        section: &'static str,
        /// Bytes the header promised.
        wanted: u64,
        /// Bytes actually available.
        got: u64,
    },

    /// The container version is not one this build reads.
    #[error("snapshot format version {found} is not supported (this build reads {expected})")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Version this build writes.
        expected: u32,
    },

    /// The endian marker did not round-trip.
    #[error("snapshot endianness marker is wrong: expected {expected:#010x}, found {found:#010x}")]
    BadEndian {
        /// Marker in the file.
        found: u32,
        /// Marker this build writes.
        expected: u32,
    },

    /// A struct width in the file disagrees with this build's layout.
    #[error("snapshot {what} is {found} bytes; this build uses {expected}")]
    LayoutMismatch {
        /// Which struct.
        what: &'static str,
        /// Width in the file.
        found: usize,
        /// Width in this build.
        expected: usize,
    },

    /// The file declares a compression this build cannot undo.
    #[error("snapshot uses compression {found}, which this build cannot read")]
    UnsupportedCompression {
        /// The byte found in the header.
        found: u8,
    },

    /// A declared length exceeds [`Limits`]. Checked before allocating.
    #[error("snapshot declares {found} {what}, over the limit of {limit}")]
    LimitExceeded {
        /// Which count.
        what: &'static str,
        /// Value in the file.
        found: u64,
        /// Ceiling in force.
        limit: u64,
    },

    /// The checksum over the file did not match the trailer.
    #[error("snapshot checksum mismatch: computed {computed:#018x}, stored {stored:#018x}")]
    ChecksumMismatch {
        /// What the bytes actually hash to.
        computed: u64,
        /// What the trailer claims.
        stored: u64,
    },

    /// The metadata section is not valid JSON for this build's schema.
    #[error("snapshot metadata is unreadable: {0}")]
    BadMetadata(String),

    /// The arena failed the same structural validation a fresh tree must pass.
    #[error("snapshot arena is not a valid tree: {0}")]
    Arena(#[from] crate::error::ArenaError),
}

/// A non-cryptographic checksum for corruption detection.
///
/// FNV-1a's structure over 64-bit little-endian **words** rather than over
/// bytes. Byte-wise FNV-1a would add roughly half a second to a 700 MB load,
/// which is a real fraction of the time this whole file exists to save; folding
/// a word at a time keeps the pass close to memory bandwidth.
///
/// It is deliberately not called FNV: it is not the standard, and nothing
/// outside this container should interoperate with it. It detects truncation,
/// bit rot, and a torn write. It is not a defence against a chosen-collision
/// attacker, and nothing here treats it as one — a snapshot is a local file
/// written by this app under the user's own account.
/// ## The one invariant that matters
///
/// **The digest is a pure function of the concatenated byte stream, and must
/// never depend on how that stream was split across [`write`](Self::write)
/// calls.** The writer emits the node, id, and totals sections in
/// [`BLOCK_NODES`]-sized blocks to bound its memory; the reader consumes them
/// with different boundaries. If chunking changed the digest, every snapshot
/// larger than one block would fail its own verification — which is precisely
/// what an earlier version of this type did, because it folded the per-call
/// length and zero-padded a per-call ragged tail.
///
/// Two mechanisms enforce it, and neither is optional:
///
/// - a partial 64-bit word is **carried across calls** in `carry`, so words are
///   cut from the global stream rather than from whichever slice arrived;
/// - the total length is folded **once, in [`finish`](Self::finish)**, not per
///   call, so that trailing zero bytes still cannot be added or removed
///   silently.
///
/// `checksum_is_independent_of_chunking` in the tests below is the guard. Do
/// not weaken it: it is the difference between a checksum and a coin flip.
#[derive(Clone, Copy, Debug)]
struct Checksum {
    state: u64,
    /// Bytes of an incomplete word held over until the next call.
    carry: [u8; 8],
    /// How many of `carry` are live.
    carry_len: usize,
    /// Every byte folded so far.
    total: u64,
}

impl Checksum {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    const fn new() -> Self {
        Self {
            state: Self::OFFSET,
            carry: [0; 8],
            carry_len: 0,
            total: 0,
        }
    }

    fn fold(&mut self, word: u64) {
        self.state = (self.state ^ word).wrapping_mul(Self::PRIME);
    }

    /// Folds `bytes` into the digest.
    ///
    /// May be called with any split of the stream; see the type documentation.
    fn write(&mut self, bytes: &[u8]) {
        self.total = self.total.wrapping_add(as_u64(bytes.len()));

        let mut rest = bytes;
        // Top up a word left incomplete by the previous call before cutting any
        // new ones, so a word never begins at a call boundary.
        if self.carry_len > 0 {
            let wanted = 8 - self.carry_len;
            let taken = wanted.min(rest.len());
            self.carry[self.carry_len..self.carry_len + taken].copy_from_slice(&rest[..taken]);
            self.carry_len += taken;
            rest = &rest[taken..];
            if self.carry_len == 8 {
                self.fold(u64::from_le_bytes(self.carry));
                self.carry_len = 0;
            }
        }

        let mut chunks = rest.chunks_exact(8);
        for chunk in &mut chunks {
            let mut word = [0_u8; 8];
            word.copy_from_slice(chunk);
            self.fold(u64::from_le_bytes(word));
        }

        let tail = chunks.remainder();
        self.carry[..tail.len()].copy_from_slice(tail);
        self.carry_len = tail.len();
    }

    /// Folds any held-over bytes and the total length, and returns the digest.
    fn finish(mut self) -> u64 {
        if self.carry_len > 0 {
            let mut word = [0_u8; 8];
            word[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
            self.fold(u64::from_le_bytes(word));
        }
        // Once, at the end. Folding it per call is what made the digest depend
        // on chunking; folding it never would make trailing zeroes free.
        self.fold(self.total);
        self.state
    }
}

/// A writer that checksums everything on its way through.
struct Tee<'a, W: Write> {
    inner: &'a mut W,
    checksum: Checksum,
}

impl<W: Write> Tee<'_, W> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), SnapshotError> {
        self.checksum.write(bytes);
        self.inner.write_all(bytes)?;
        Ok(())
    }
}

/// Every scalar of a [`CompletedScan`] — everything except the arena.
///
/// `CompletedScan` is deliberately not `Serialize` (it owns the whole tree, and
/// 69M nodes through serde would be neither fast nor bounded), so this mirrors
/// its fields. The arena travels as the binary sections; this travels as JSON
/// because it is small, self-describing, and survives a field being added.
///
/// `root_path` is raw bytes, never a `String`: a macOS path is not required to
/// be UTF-8, and a snapshot that lossily re-encoded it would resolve actions
/// against the wrong file.
#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    scan_id: u64,
    generation: u64,
    root_path: Vec<u8>,
    root: u32,
    volume: VolumeId,
    started_unix_ms: i64,
    finished_unix_ms: i64,
    options: ScanOptions,
    exclusion_hash: ConfigHash,
    category_config_hash: ConfigHash,
    tool_version: String,
    counts: ScanCounts,
    totals: ScanTotals,
    mutations: u64,
    errors: Vec<ScanError>,
    error_counts: Vec<ErrorClassCount>,
    excluded_roots: Vec<DisplayPath>,
}

/// Encodes `scan` into `out` and returns the number of bytes written.
///
/// The caller owns atomicity. This writes a byte stream in one forward pass and
/// never seeks, so it can be pointed at a temporary file that is fsynced and
/// renamed into place — which is what the shell does, and the only way a reader
/// is guaranteed never to observe a half-written snapshot.
///
/// # Errors
///
/// [`SnapshotError::Io`] if `out` fails. Encoding itself cannot fail: every
/// value comes from an already-validated frozen tree.
pub fn write<W: Write>(scan: &CompletedScan, out: &mut W) -> Result<u64, SnapshotError> {
    let tree = &scan.tree;
    let meta = Meta {
        scan_id: scan.scan_id.get(),
        generation: scan.generation.get(),
        root_path: path_to_bytes(&scan.root_path),
        root: scan.root.raw(),
        volume: scan.volume.clone(),
        started_unix_ms: scan.started_unix_ms,
        finished_unix_ms: scan.finished_unix_ms,
        options: scan.options.clone(),
        exclusion_hash: scan.exclusion_hash.clone(),
        category_config_hash: scan.category_config_hash.clone(),
        tool_version: scan.tool_version.clone(),
        counts: scan.counts,
        totals: scan.totals,
        mutations: scan.mutations,
        errors: scan.errors.clone(),
        error_counts: scan.error_counts.clone(),
        excluded_roots: scan.excluded_roots.clone(),
    };
    let meta_bytes = serde_json::to_vec(&meta).map_err(|error| SnapshotError::BadMetadata(error.to_string()))?;

    let nodes = tree.nodes();
    let names = tree.names().as_slice();
    let dirs = tree.dirs();

    let mut header = [0_u8; HEADER_BYTES];
    header[0..8].copy_from_slice(&MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&ENDIAN_MARKER.to_le_bytes());
    header[16] = u8::try_from(core::mem::size_of::<usize>()).unwrap_or(u8::MAX);
    header[17] = u8::try_from(NODE_BYTES).unwrap_or(u8::MAX);
    header[18] = u8::try_from(TOTALS_BYTES).unwrap_or(u8::MAX);
    header[19] = NO_COMPRESSION;
    // header[20..24] stays reserved-zero.
    header[24..32].copy_from_slice(&as_u64(meta_bytes.len()).to_le_bytes());
    header[32..40].copy_from_slice(&as_u64(nodes.len()).to_le_bytes());
    header[40..48].copy_from_slice(&as_u64(names.len()).to_le_bytes());
    header[48..56].copy_from_slice(&as_u64(dirs.len()).to_le_bytes());

    let mut sink = Tee {
        inner: out,
        checksum: Checksum::new(),
    };
    sink.put(&header)?;
    sink.put(&meta_bytes)?;

    // Encoded in bounded blocks rather than one allocation the size of the
    // arena: at the design profile the node array alone is 3.08 GiB, and a
    // snapshot write must not double the process's peak RSS.
    let mut block = Vec::with_capacity(BLOCK_NODES * NODE_BYTES);
    for chunk in nodes.chunks(BLOCK_NODES) {
        block.clear();
        for node in chunk {
            put_node(&mut block, node);
        }
        sink.put(&block)?;
    }

    sink.put(names)?;

    block.clear();
    block.reserve(BLOCK_NODES * NODE_ID_BYTES);
    for chunk in dirs.ids().chunks(BLOCK_NODES) {
        block.clear();
        for id in chunk {
            block.extend_from_slice(&id.raw().to_le_bytes());
        }
        sink.put(&block)?;
    }

    block.clear();
    block.reserve(BLOCK_NODES * TOTALS_BYTES);
    for chunk in dirs.totals().chunks(BLOCK_NODES) {
        block.clear();
        for totals in chunk {
            put_totals(&mut block, totals);
        }
        sink.put(&block)?;
    }

    let checksum = sink.checksum.finish();
    out.write_all(&checksum.to_le_bytes())?;

    let written = HEADER_BYTES
        .saturating_add(meta_bytes.len())
        .saturating_add(nodes.len().saturating_mul(NODE_BYTES))
        .saturating_add(names.len())
        .saturating_add(dirs.len().saturating_mul(NODE_ID_BYTES))
        .saturating_add(dirs.len().saturating_mul(TOTALS_BYTES))
        .saturating_add(TRAILER_BYTES);
    Ok(as_u64(written))
}

/// Decodes a snapshot from `input`.
///
/// The returned scan carries the `scan_id` and `generation` the file was
/// written with. Those are **per-process** counters, so a caller that publishes
/// a loaded snapshot into a running app must re-stamp both before publishing —
/// otherwise a stale generation from a previous run could collide with a live
/// one and a read command would answer against the wrong tree.
///
/// # Errors
///
/// Any [`SnapshotError`]. The file is untrusted: nothing is allocated on a
/// declared length until it passes `limits`, and the arena is not returned
/// until [`Tree::from_parts`] has validated every structural invariant.
pub fn read<R: Read>(input: &mut R, limits: Limits) -> Result<CompletedScan, SnapshotError> {
    let mut checksum = Checksum::new();

    let mut raw_header = [0_u8; HEADER_BYTES];
    fill(input, &mut raw_header, "header")?;
    checksum.write(&raw_header);
    let Header {
        meta_len,
        node_count,
        name_len,
        dir_count,
    } = parse_header(&raw_header, limits)?;

    let meta_bytes = take(input, meta_len, "metadata", &mut checksum)?;
    let meta: Meta =
        serde_json::from_slice(&meta_bytes).map_err(|error| SnapshotError::BadMetadata(error.to_string()))?;

    // Each of the three arrays below is decoded as it streams, never staged
    // whole. Staging the node section would hold its encoded bytes and the
    // parsed `Vec<Node>` at the same time — 3.08 GiB each at the design
    // profile, so 6.16 GiB against a 5.0 GiB ceiling
    // (docs/01-ARCHITECTURE.md#memory-budget). The write path already refuses
    // to do this for the same reason; the read path is the larger of the two.
    let nodes = take_records(input, node_count, NODE_BYTES, "nodes", &mut checksum, get_node)?;

    // The name blob is the exception, and legitimately so: the bytes read *are*
    // the final storage, so there is no second copy to avoid.
    let names = NameBlob::from_vec(take(input, name_len, "names", &mut checksum)?);

    let dir_ids = take_records(input, dir_count, NODE_ID_BYTES, "directory ids", &mut checksum, |raw| {
        NodeId::from_raw(u32_at(raw, 0))
    })?;

    let dir_totals = take_records(
        input,
        dir_count,
        TOTALS_BYTES,
        "directory totals",
        &mut checksum,
        get_totals,
    )?;

    let mut trailer = [0_u8; TRAILER_BYTES];
    fill(input, &mut trailer, "checksum")?;
    let stored = u64::from_le_bytes(trailer);
    let computed = checksum.finish();
    if computed != stored {
        return Err(SnapshotError::ChecksumMismatch { computed, stored });
    }

    // Only now, with the bytes proven intact, is the arena assembled — and
    // `from_parts` still re-checks every name range, link, ordering, and depth
    // invariant, because an intact file can still have been written by a build
    // with a bug.
    let dirs = DirIndex::from_parts(dir_ids, dir_totals)?;
    let root = NodeId::from_raw(meta.root);
    let tree = Tree::from_parts(nodes, names, dirs, root)?;

    Ok(CompletedScan {
        scan_id: ScanId::from_raw(meta.scan_id),
        generation: TreeGeneration::from_raw(meta.generation),
        root_path: bytes_to_path(meta.root_path),
        root,
        volume: meta.volume,
        started_unix_ms: meta.started_unix_ms,
        finished_unix_ms: meta.finished_unix_ms,
        options: meta.options,
        exclusion_hash: meta.exclusion_hash,
        category_config_hash: meta.category_config_hash,
        tool_version: meta.tool_version,
        counts: meta.counts,
        totals: meta.totals,
        mutations: meta.mutations,
        errors: meta.errors,
        error_counts: meta.error_counts,
        excluded_roots: meta.excluded_roots,
        tree,
    })
}

/// What a snapshot says about itself, without decoding its arena.
///
/// Everything here comes from the fixed header and the small JSON metadata
/// section — the first few kilobytes of the file. Deciding whether to offer
/// "restore" in a menu must not cost the 77 MB read that actually restoring
/// would, and a menu that renders on every open cannot pay for an arena.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Peek {
    /// Retained nodes in the arena.
    pub nodes: u64,
    /// Directories in the arena.
    pub directories: u64,
    /// The scanned root.
    pub root_path: PathBuf,
    /// Which volume it was.
    pub volume: VolumeId,
    /// When the scan finished, Unix milliseconds.
    pub finished_unix_ms: i64,
    /// Logical and allocated totals.
    pub totals: ScanTotals,
    /// The build that wrote it.
    pub tool_version: String,
}

/// Reads a snapshot's header and metadata, and stops.
///
/// The arena is **not** read, so this returns in the time it takes to read a
/// few kilobytes regardless of how large the file is. The checksum covers the
/// whole file and therefore cannot be verified here; a `Peek` is a label, not a
/// guarantee, and [`read`] still validates everything before a tree is built
/// from it.
///
/// # Errors
///
/// The header errors from [`read`] — magic, version, endianness, layout,
/// compression, limits — plus [`SnapshotError::BadMetadata`].
pub fn peek<R: Read>(input: &mut R, limits: Limits) -> Result<Peek, SnapshotError> {
    let mut raw_header = [0_u8; HEADER_BYTES];
    fill(input, &mut raw_header, "header")?;
    let header = parse_header(&raw_header, limits)?;

    let mut discard = Checksum::new();
    let meta_bytes = take(input, header.meta_len, "metadata", &mut discard)?;
    let meta: Meta =
        serde_json::from_slice(&meta_bytes).map_err(|error| SnapshotError::BadMetadata(error.to_string()))?;

    Ok(Peek {
        nodes: header.node_count,
        directories: header.dir_count,
        root_path: bytes_to_path(meta.root_path),
        volume: meta.volume,
        finished_unix_ms: meta.finished_unix_ms,
        totals: meta.totals,
        tool_version: meta.tool_version,
    })
}

/// The four section lengths, once the header has been proven to describe a
/// container this build can read.
struct Header {
    meta_len: u64,
    node_count: u64,
    name_len: u64,
    dir_count: u64,
}

/// Validates the fixed header and returns the section lengths.
///
/// Kept separate from [`read`] so that every reason to reject a file sits in
/// one place, in the order a reader must apply them: identity first, then
/// version, then this build's layout, then plausibility. A length is not
/// returned at all until it has passed `limits`, so the caller cannot allocate
/// on a value that was never checked.
fn parse_header(raw: &[u8; HEADER_BYTES], limits: Limits) -> Result<Header, SnapshotError> {
    if raw[0..8] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = u32_at(raw, 8);
    if version != FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedVersion {
            found: version,
            expected: FORMAT_VERSION,
        });
    }
    let endian = u32_at(raw, 12);
    if endian != ENDIAN_MARKER {
        return Err(SnapshotError::BadEndian {
            found: endian,
            expected: ENDIAN_MARKER,
        });
    }
    if usize::from(raw[17]) != NODE_BYTES {
        return Err(SnapshotError::LayoutMismatch {
            what: "Node",
            found: usize::from(raw[17]),
            expected: NODE_BYTES,
        });
    }
    if usize::from(raw[18]) != TOTALS_BYTES {
        return Err(SnapshotError::LayoutMismatch {
            what: "DirTotals",
            found: usize::from(raw[18]),
            expected: TOTALS_BYTES,
        });
    }
    if raw[19] != NO_COMPRESSION {
        return Err(SnapshotError::UnsupportedCompression { found: raw[19] });
    }

    let header = Header {
        meta_len: u64_at(raw, 24),
        node_count: u64_at(raw, 32),
        name_len: u64_at(raw, 40),
        dir_count: u64_at(raw, 48),
    };

    // Every ceiling is checked before a single byte is reserved, so a corrupt
    // length is a rejection rather than an allocation the machine cannot serve.
    check(header.meta_len, limits.max_meta_bytes, "metadata bytes")?;
    check(header.node_count, limits.max_nodes, "nodes")?;
    check(header.name_len, limits.max_name_bytes, "name bytes")?;
    check(header.dir_count, limits.max_dirs, "directories")?;

    Ok(header)
}

// --- encoding helpers -------------------------------------------------------

fn put_node(out: &mut Vec<u8>, node: &Node) {
    out.extend_from_slice(&node.size.to_le_bytes());
    out.extend_from_slice(&node.alloc.to_le_bytes());
    out.extend_from_slice(&node.mtime.to_le_bytes());
    out.extend_from_slice(&node.name.raw().to_le_bytes());
    out.extend_from_slice(&node.parent.raw().to_le_bytes());
    out.extend_from_slice(&node.first_child.raw().to_le_bytes());
    out.extend_from_slice(&node.next_sibling.raw().to_le_bytes());
    out.extend_from_slice(&node.flags.to_le_bytes());
    out.push(node.category);
    out.push(node.kind);
}

fn get_node(raw: &[u8]) -> Node {
    Node {
        size: u64_at(raw, 0),
        alloc: u64_at(raw, 8),
        mtime: i64_at(raw, 16),
        name: NameRef::from_raw(u64_at(raw, 24)),
        parent: NodeId::from_raw(u32_at(raw, 32)),
        first_child: NodeId::from_raw(u32_at(raw, 36)),
        next_sibling: NodeId::from_raw(u32_at(raw, 40)),
        flags: u16_at(raw, 44),
        category: raw[46],
        kind: raw[47],
    }
}

fn put_totals(out: &mut Vec<u8>, totals: &DirTotals) {
    out.extend_from_slice(&totals.logical.to_le_bytes());
    out.extend_from_slice(&totals.allocated.to_le_bytes());
    out.extend_from_slice(&totals.direct_logical.to_le_bytes());
    out.extend_from_slice(&totals.direct_allocated.to_le_bytes());
    out.extend_from_slice(&totals.latest_mtime.to_le_bytes());
    out.extend_from_slice(&totals.observed_entries.to_le_bytes());
    out.extend_from_slice(&totals.retained_nodes.to_le_bytes());
    out.extend_from_slice(&totals.direct_files.to_le_bytes());
    out.extend_from_slice(&totals.unreadable.to_le_bytes());
}

fn get_totals(raw: &[u8]) -> DirTotals {
    DirTotals {
        logical: u64_at(raw, 0),
        allocated: u64_at(raw, 8),
        direct_logical: u64_at(raw, 16),
        direct_allocated: u64_at(raw, 24),
        latest_mtime: i64_at(raw, 32),
        observed_entries: u32_at(raw, 40),
        retained_nodes: u32_at(raw, 44),
        direct_files: u32_at(raw, 48),
        unreadable: u32_at(raw, 52),
    }
}

// Fixed-offset readers. Every call site indexes a slice this module just sized
// itself, so a short read is a bug here rather than untrusted input; the
// `unwrap_or` keeps that from being a panic in a crate that denies them.
fn u16_at(raw: &[u8], offset: usize) -> u16 {
    raw.get(offset..offset + 2)
        .and_then(|slice| slice.try_into().ok())
        .map_or(0, u16::from_le_bytes)
}

fn u32_at(raw: &[u8], offset: usize) -> u32 {
    raw.get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

fn u64_at(raw: &[u8], offset: usize) -> u64 {
    raw.get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

fn i64_at(raw: &[u8], offset: usize) -> i64 {
    raw.get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .map_or(0, i64::from_le_bytes)
}

/// Widens a length for the wire. `usize` is 64-bit on every target this ships
/// to, so the saturation is unreachable there; it exists because the workspace
/// denies truncating casts and a length is never worth an `as`.
fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn check(found: u64, limit: u64, what: &'static str) -> Result<(), SnapshotError> {
    if found > limit {
        return Err(SnapshotError::LimitExceeded { what, found, limit });
    }
    Ok(())
}

/// Reads exactly `buffer.len()` bytes, or reports which section ran short.
fn fill<R: Read>(input: &mut R, buffer: &mut [u8], section: &'static str) -> Result<(), SnapshotError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match input.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(SnapshotError::Truncated {
                    section,
                    wanted: as_u64(buffer.len()),
                    got: as_u64(filled),
                });
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(SnapshotError::Io(error)),
        }
    }
    Ok(())
}

/// Reads `count` fixed-width records, decoding each block as it arrives.
///
/// The staging buffer is one block — [`BLOCK_NODES`] records, a few hundred
/// kilobytes — regardless of how large the section is. Only the decoded `Vec`
/// grows with the arena, so peak memory is the arena itself rather than the
/// arena plus a second encoded copy of it.
///
/// The per-block `checksum.write` calls do not have to line up with the
/// writer's blocks; [`Checksum`] is explicitly independent of chunking, and the
/// test below holds it to that.
fn take_records<R: Read, T>(
    input: &mut R,
    count: u64,
    width: usize,
    section: &'static str,
    checksum: &mut Checksum,
    decode: impl Fn(&[u8]) -> T,
) -> Result<Vec<T>, SnapshotError> {
    let count = usize::try_from(count).map_err(|_| SnapshotError::LimitExceeded {
        what: section,
        found: count,
        limit: as_u64(usize::MAX),
    })?;

    let mut out = Vec::with_capacity(count);
    let mut block = vec![0_u8; BLOCK_NODES.saturating_mul(width)];
    let mut remaining = count;
    while remaining > 0 {
        let this_block = remaining.min(BLOCK_NODES);
        let slice = block
            .get_mut(..this_block.saturating_mul(width))
            .ok_or(SnapshotError::LimitExceeded {
                what: section,
                found: as_u64(this_block),
                limit: as_u64(BLOCK_NODES),
            })?;
        fill(input, slice, section)?;
        checksum.write(slice);
        for record in slice.chunks_exact(width) {
            out.push(decode(record));
        }
        remaining -= this_block;
    }
    Ok(out)
}

/// Reads a whole section that has already been bounds-checked against
/// [`Limits`], folding it into the running checksum.
fn take<R: Read>(
    input: &mut R,
    length: u64,
    section: &'static str,
    checksum: &mut Checksum,
) -> Result<Vec<u8>, SnapshotError> {
    let length = usize::try_from(length).map_err(|_| SnapshotError::LimitExceeded {
        what: section,
        found: length,
        limit: as_u64(usize::MAX),
    })?;
    let mut buffer = vec![0_u8; length];
    fill(input, &mut buffer, section)?;
    checksum.write(&buffer);
    Ok(buffer)
}

// --- path bytes -------------------------------------------------------------
//
// A macOS path is a byte string, not UTF-8. `OsStr::as_encoded_bytes` is the
// portable way out, but its inverse is `unsafe`, and this crate forbids that.
// On Unix `OsStrExt::from_bytes` is the safe inverse, so the round trip is
// lossless on the platform this ships to; elsewhere it degrades to UTF-8, which
// is only exercised by portability builds and never by a shipped snapshot.

#[cfg(unix)]
fn path_to_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_to_bytes(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "a test that cannot build its own fixture has already failed"
)]
mod tests {
    use super::*;
    use crate::dirs::DirTotals;
    use crate::id::CategoryId;
    use crate::node::{Kind, flags};
    use crate::tree::TreeBuilder;

    /// root/
    ///   media/          (dir)
    ///     clip.mkv      4 KiB logical, 8 KiB allocated, hard-linked
    ///     naïve ☕.txt   deliberately non-ASCII, to prove the blob is bytes
    ///   readme.txt      100 B
    fn scan() -> CompletedScan {
        let mut builder = TreeBuilder::new();

        let root_name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(root_name, 10)).expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        let media_name = builder.intern(b"media").expect("interns");
        let media = builder
            .push_child(root, Node::directory(media_name, 20))
            .expect("links");
        builder.register_directory(media, DirTotals::EMPTY).expect("registers");

        let clip_name = builder.intern(b"clip.mkv").expect("interns");
        let clip = Node::leaf(clip_name, Kind::File, 4096, 8192, 30)
            .with_flags(flags::HARD_LINK)
            .with_category(CategoryId::from_raw(3));
        builder.push_child(media, clip).expect("links");
        if let Some(totals) = builder.dir_totals_mut(media) {
            totals.absorb_direct_file(clip.contributed_size(), clip.contributed_alloc(), 30);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }

        // A name that is not ASCII and not shell-safe: the blob must carry the
        // bytes through untouched, because an action resolves against them.
        let odd_name = builder.intern("naïve ☕.txt".as_bytes()).expect("interns");
        let odd = Node::leaf(odd_name, Kind::File, 7, 4096, -5);
        builder.push_child(media, odd).expect("links");
        if let Some(totals) = builder.dir_totals_mut(media) {
            totals.absorb_direct_file(odd.contributed_size(), odd.contributed_alloc(), -5);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }

        let readme_name = builder.intern(b"readme.txt").expect("interns");
        let readme = Node::leaf(readme_name, Kind::File, 100, 4096, 40);
        builder.push_child(root, readme).expect("links");
        if let Some(totals) = builder.dir_totals_mut(root) {
            totals.absorb_direct_file(readme.contributed_size(), readme.contributed_alloc(), 40);
            totals.observed_entries += 1;
            totals.retained_nodes += 1;
        }

        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");

        CompletedScan {
            scan_id: ScanId::from_raw(7),
            generation: TreeGeneration::from_raw(3),
            root_path: PathBuf::from("/Volumes/Macintosh HD"),
            root,
            volume: VolumeId {
                device: 16_777_232,
                fs_type: "apfs".to_owned(),
                volume_uuid: Some("D7C1-4E".to_owned()),
                mount_point: DisplayPath::from_bytes(b"/"),
                case_preserving: true,
                case_sensitive: false,
            },
            started_unix_ms: 1_700_000_000_000,
            finished_unix_ms: 1_700_000_191_000,
            options: ScanOptions::default(),
            exclusion_hash: ConfigHash::from_hex("abc123".to_owned()),
            category_config_hash: ConfigHash::from_hex("def456".to_owned()),
            tool_version: "0.1.0-test".to_owned(),
            counts: ScanCounts::default(),
            totals: ScanTotals {
                logical: 4203,
                allocated: 16_384,
            },
            mutations: 2,
            errors: Vec::new(),
            error_counts: Vec::new(),
            excluded_roots: vec![DisplayPath::from_bytes(b"/System/Volumes/Data")],
            tree,
        }
    }

    fn encode(scan: &CompletedScan) -> Vec<u8> {
        let mut buffer = Vec::new();
        let reported = write(scan, &mut buffer).expect("writes");
        // The writer's own byte count must match what it actually emitted, or
        // the store's size accounting and retention maths are built on a lie.
        assert_eq!(reported, as_u64(buffer.len()), "reported length disagrees with output");
        buffer
    }

    fn decode(bytes: &[u8]) -> Result<CompletedScan, SnapshotError> {
        read(&mut &bytes[..], Limits::default())
    }

    #[test]
    fn a_snapshot_round_trips_the_whole_arena() {
        let original = scan();
        let restored = decode(&encode(&original)).expect("reads");

        assert_eq!(restored.tree, original.tree, "arena differs after a round trip");
        assert_eq!(restored.root_path, original.root_path);
        assert_eq!(restored.scan_id, original.scan_id);
        assert_eq!(restored.generation, original.generation);
        assert_eq!(restored.volume, original.volume);
        assert_eq!(restored.totals, original.totals);
        assert_eq!(restored.mutations, original.mutations);
        assert_eq!(restored.excluded_roots, original.excluded_roots);
        assert_eq!(restored.tool_version, original.tool_version);
    }

    #[test]
    fn names_survive_as_bytes_not_as_text() {
        let original = scan();
        let restored = decode(&encode(&original)).expect("reads");

        let names: Vec<&[u8]> = restored
            .tree
            .nodes()
            .iter()
            .filter_map(|node| restored.tree.names().bytes(node.name))
            .collect();
        assert!(
            names.contains(&"naïve ☕.txt".as_bytes()),
            "a non-ASCII name did not survive the round trip: {names:?}"
        );
    }

    #[test]
    fn a_negative_mtime_is_not_clamped() {
        let restored = decode(&encode(&scan())).expect("reads");
        let found = restored.tree.nodes().iter().any(|node| node.mtime == -5);
        assert!(found, "a pre-epoch mtime was lost");
    }

    // --- corruption fixtures -------------------------------------------------
    //
    // Each mutates one byte or field of an otherwise-valid file. The point is
    // not merely that reading fails, but that it fails with the error that
    // tells the caller what to do next.

    #[test]
    fn a_file_that_is_not_a_snapshot_is_refused() {
        let mut bytes = encode(&scan());
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes), Err(SnapshotError::BadMagic)));
    }

    #[test]
    fn a_future_format_version_is_refused_rather_than_partly_read() {
        let mut bytes = encode(&scan());
        bytes[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(SnapshotError::UnsupportedVersion { found, .. }) if found == FORMAT_VERSION + 1
        ));
    }

    #[test]
    fn a_byte_swapped_file_is_refused_on_the_header() {
        let mut bytes = encode(&scan());
        bytes[12..16].copy_from_slice(&ENDIAN_MARKER.swap_bytes().to_le_bytes());
        assert!(matches!(decode(&bytes), Err(SnapshotError::BadEndian { .. })));
    }

    #[test]
    fn a_node_width_from_another_build_is_refused() {
        let mut bytes = encode(&scan());
        bytes[17] = 56;
        assert!(matches!(
            decode(&bytes),
            Err(SnapshotError::LayoutMismatch { what: "Node", .. })
        ));
    }

    #[test]
    fn an_unknown_compression_is_refused_rather_than_guessed() {
        let mut bytes = encode(&scan());
        bytes[19] = 9;
        assert!(matches!(
            decode(&bytes),
            Err(SnapshotError::UnsupportedCompression { found: 9 })
        ));
    }

    #[test]
    fn an_absurd_node_count_is_refused_before_anything_is_allocated() {
        let mut bytes = encode(&scan());
        // A corrupt length that would otherwise reserve ~750 TiB of nodes.
        bytes[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            decode(&bytes),
            Err(SnapshotError::LimitExceeded { what: "nodes", .. })
        ));
    }

    #[test]
    fn a_truncated_file_names_the_section_that_ran_short() {
        let bytes = encode(&scan());
        let cut = bytes.len() - 16;
        assert!(matches!(decode(&bytes[..cut]), Err(SnapshotError::Truncated { .. })));
    }

    #[test]
    fn an_empty_file_is_refused() {
        assert!(matches!(decode(&[]), Err(SnapshotError::Truncated { .. })));
    }

    #[test]
    fn a_single_flipped_bit_in_the_arena_is_caught_by_the_checksum() {
        let mut bytes = encode(&scan());
        // Land in the node section: past the header and the JSON metadata.
        let target = bytes.len() - TRAILER_BYTES - 4;
        bytes[target] ^= 0b0000_0001;
        assert!(
            matches!(decode(&bytes), Err(SnapshotError::ChecksumMismatch { .. })),
            "a flipped bit in the payload was not detected"
        );
    }

    #[test]
    fn a_tampered_checksum_does_not_validate_a_tampered_payload() {
        let mut bytes = encode(&scan());
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(matches!(decode(&bytes), Err(SnapshotError::ChecksumMismatch { .. })));
    }

    #[test]
    fn unparseable_metadata_is_reported_as_metadata_not_as_corruption() {
        let scan = scan();
        let mut bytes = encode(&scan);
        let meta_len = usize::try_from(u64_at(&bytes, 24)).expect("fits");
        // Overwrite the JSON in place, then re-stamp the checksum, so the only
        // thing wrong with the file is the metadata itself.
        let meta_start = HEADER_BYTES;
        let filler = b'!';
        for byte in &mut bytes[meta_start..meta_start + meta_len] {
            *byte = filler;
        }
        let mut checksum = Checksum::new();
        checksum.write(&bytes[..HEADER_BYTES]);
        checksum.write(&bytes[meta_start..meta_start + meta_len]);
        let mut offset = meta_start + meta_len;
        let node_count = usize::try_from(u64_at(&bytes, 32)).expect("fits");
        let name_len = usize::try_from(u64_at(&bytes, 40)).expect("fits");
        let dir_count = usize::try_from(u64_at(&bytes, 48)).expect("fits");
        for length in [
            node_count * NODE_BYTES,
            name_len,
            dir_count * NODE_ID_BYTES,
            dir_count * TOTALS_BYTES,
        ] {
            checksum.write(&bytes[offset..offset + length]);
            offset += length;
        }
        bytes[offset..offset + TRAILER_BYTES].copy_from_slice(&checksum.finish().to_le_bytes());

        assert!(
            matches!(decode(&bytes), Err(SnapshotError::BadMetadata(_))),
            "bad metadata should be distinguishable from a damaged file"
        );
    }

    /// Builds a root with `files` children, to push a section past one
    /// [`BLOCK_NODES`] block.
    fn wide_scan(files: usize) -> CompletedScan {
        let mut builder = TreeBuilder::new();
        let root_name = builder.intern(b"root").expect("interns");
        let root = builder.push_node(Node::directory(root_name, 10)).expect("pushes");
        builder.register_directory(root, DirTotals::EMPTY).expect("registers");

        for index in 0..files {
            let name = builder.intern(format!("f{index:07}.bin").as_bytes()).expect("interns");
            let size = u64::try_from(index).expect("a test index fits");
            let mtime = i64::try_from(index).expect("a test index fits");
            let leaf = Node::leaf(name, Kind::File, size, 4096, mtime);
            builder.push_child(root, leaf).expect("links");
            if let Some(totals) = builder.dir_totals_mut(root) {
                totals.absorb_direct_file(leaf.contributed_size(), leaf.contributed_alloc(), mtime);
                totals.observed_entries += 1;
                totals.retained_nodes += 1;
            }
        }

        builder.rollup().expect("rolls up");
        let tree = builder.finish().expect("valid");
        let mut scan = scan();
        scan.root = root;
        scan.tree = tree;
        scan
    }

    /// The regression test for the defect this format shipped with.
    ///
    /// The writer emits the node section in `BLOCK_NODES`-sized blocks; the
    /// reader consumes it with its own boundaries. A checksum that folded a
    /// per-call length made those two disagree, so **every snapshot larger than
    /// one block failed its own verification** — which is every real scan, the
    /// design profile being 69M nodes.
    ///
    /// It went unnoticed because every other fixture here is a handful of
    /// nodes, three orders of magnitude under the block size. One node either
    /// side of the boundary is the whole point: at exactly `BLOCK_NODES` there
    /// is a single block and the bug is invisible.
    #[test]
    fn a_snapshot_larger_than_one_block_round_trips() {
        for files in [BLOCK_NODES - 1, BLOCK_NODES, BLOCK_NODES + 1, BLOCK_NODES * 3 + 7] {
            let original = wide_scan(files);
            let restored = decode(&encode(&original))
                .unwrap_or_else(|error| panic!("a {files}-file snapshot failed to read back: {error}"));
            assert_eq!(
                restored.tree, original.tree,
                "the arena differs after a round trip at {files} files"
            );
        }
    }

    /// The invariant the fix rests on, tested directly rather than only through
    /// a round trip: the digest is a function of the bytes, not of how they
    /// were handed over.
    #[test]
    fn checksum_is_independent_of_chunking() {
        let bytes: Vec<u8> = (0..1000_u32).map(|index| (index % 251) as u8).collect();

        let mut whole = Checksum::new();
        whole.write(&bytes);
        let expected = whole.finish();

        // Splits chosen to land off any 8-byte boundary, since a word that
        // straddles a call is exactly what the carry buffer exists for.
        for split in [1_usize, 3, 7, 8, 9, 15, 16, 17, 100, 333, 999] {
            let mut parts = Checksum::new();
            parts.write(&bytes[..split]);
            parts.write(&bytes[split..]);
            assert_eq!(
                parts.finish(),
                expected,
                "the digest changed when the stream was split at {split}"
            );
        }

        // And for many small writes, which is the writer's actual shape.
        let mut many = Checksum::new();
        for chunk in bytes.chunks(7) {
            many.write(chunk);
        }
        assert_eq!(
            many.finish(),
            expected,
            "the digest changed under repeated small writes"
        );
    }

    #[test]
    fn the_checksum_notices_a_reordering_that_preserves_every_byte() {
        // Corruption that keeps the byte *multiset* — two words swapped by a
        // bad write — must still change the digest. This is the property the
        // old "section boundaries are checksummed" assertion was reaching for;
        // that one tested the bug instead, because it required the digest to
        // depend on how the stream was chunked.
        let mut forwards = Checksum::new();
        forwards.write(b"aaaaaaaabbbbbbbb");
        let mut backwards = Checksum::new();
        backwards.write(b"bbbbbbbbaaaaaaaa");
        assert_ne!(forwards.finish(), backwards.finish(), "two swapped words hash the same");
    }

    #[test]
    fn trailing_zero_bytes_change_the_checksum() {
        let mut a = Checksum::new();
        a.write(b"ab");
        let mut b = Checksum::new();
        b.write(b"ab\0\0");
        assert_ne!(a.finish(), b.finish(), "zero padding is indistinguishable from data");
    }

    #[test]
    fn a_peek_reports_the_scan_without_reading_the_arena() {
        let original = scan();
        let bytes = encode(&original);
        let peeked = peek(&mut &bytes[..], Limits::default()).expect("peeks");

        assert_eq!(peeked.nodes, as_u64(original.tree.len()));
        assert_eq!(peeked.root_path, original.root_path);
        assert_eq!(peeked.finished_unix_ms, original.finished_unix_ms);
        assert_eq!(peeked.totals, original.totals);
        assert_eq!(peeked.volume, original.volume);
    }

    #[test]
    fn a_peek_stops_before_the_arena() {
        // Truncated immediately after the metadata: `read` must fail and `peek`
        // must not. That is the whole point — a label costs a few kilobytes
        // whatever the file weighs.
        let bytes = encode(&scan());
        let meta_len = usize::try_from(u64_at(&bytes, 24)).expect("fits");
        let prefix = &bytes[..HEADER_BYTES + meta_len];

        assert!(peek(&mut &prefix[..], Limits::default()).is_ok());
        assert!(read(&mut &prefix[..], Limits::default()).is_err());
    }

    #[test]
    fn a_peek_still_refuses_a_file_it_does_not_understand() {
        let mut bytes = encode(&scan());
        bytes[0] = b'X';
        assert!(matches!(
            peek(&mut &bytes[..], Limits::default()),
            Err(SnapshotError::BadMagic)
        ));
    }
}
