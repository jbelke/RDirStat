//! The name blob and its packed reference.
//!
//! macOS path components are retained as **lossless filesystem bytes**. There is
//! one append-only blob per tree and every [`Node`](crate::Node) carries a
//! 64-bit [`NameRef`] into it, so 69M names cost 8 bytes of pointer each instead
//! of a `String` header plus an allocation.
//!
//! Full paths are not stored per node; they are reconstructed by walking
//! parents ([`Tree::path_bytes`](crate::Tree::path_bytes)).
//!
//! Every read out of the blob goes through [`NameBlob::bytes`], which validates
//! the range against the blob length and returns `Option`. There is no
//! unchecked slicing anywhere in this crate and commands must not add one: a
//! snapshot file is attacker-influenced input, and a bad `NameRef` in it must
//! fail closed rather than read adjacent names.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ArenaError;

/// Largest offset a [`NameRef`] can address: the blob is capped at 256 TiB.
pub const MAX_NAME_OFFSET: u64 = (1 << 48) - 1;

/// Largest single name, in bytes. `NAME_MAX` on Darwin is 255, so the 16-bit
/// field is generous by two orders of magnitude.
pub const MAX_NAME_LEN: usize = u16::MAX as usize;

const OFFSET_MASK: u64 = MAX_NAME_OFFSET;
const LEN_SHIFT: u32 = 48;

/// A packed reference into a [`NameBlob`]: low 48 bits are the byte offset,
/// high 16 bits are the byte length.
///
/// Construction is checked ([`NameRef::new`]) and reading is checked
/// ([`NameBlob::bytes`]). A `NameRef` on its own is inert — it cannot deref,
/// slice, or otherwise touch memory.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type)]
#[repr(transparent)]
#[serde(transparent)]
pub struct NameRef(u64);

impl NameRef {
    /// A zero-length name at offset zero. Used for the root, whose displayed
    /// name comes from the scan root path rather than from the blob.
    pub const EMPTY: Self = Self(0);

    /// Packs `offset` and `len`.
    ///
    /// # Errors
    ///
    /// [`ArenaError::NameOffsetOverflow`] if `offset > MAX_NAME_OFFSET`.
    #[allow(clippy::cast_lossless, reason = "u16 -> u64 is widening; `u64::from` is not const")]
    pub const fn new(offset: u64, len: u16) -> Result<Self, ArenaError> {
        if offset > MAX_NAME_OFFSET {
            return Err(ArenaError::NameOffsetOverflow { offset });
        }
        Ok(Self((offset & OFFSET_MASK) | ((len as u64) << LEN_SHIFT)))
    }

    /// Reinterprets a raw wire or snapshot value without validating it.
    ///
    /// Validation happens when the reference is resolved against a blob. A
    /// `NameRef` read out of a `*.rdstat` file is never trusted until
    /// [`NameBlob::bytes`] accepts it.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw packed value, for snapshot writing.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Byte offset of the name within its blob.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.0 & OFFSET_MASK
    }

    /// Byte length of the name.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "shifting down by 48 leaves exactly 16 bits"
    )]
    #[must_use]
    pub const fn len(self) -> u16 {
        (self.0 >> LEN_SHIFT) as u16
    }

    /// Whether the name is zero bytes long.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Exclusive end offset, as a `u64` so the addition cannot overflow.
    #[must_use]
    pub fn end(self) -> u64 {
        self.offset() + u64::from(self.len())
    }
}

impl Default for NameRef {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl fmt::Debug for NameRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NameRef({}..+{})", self.offset(), self.len())
    }
}

/// One append-only byte arena holding every retained name in a tree.
///
/// Names are stored exactly as the filesystem produced them. Nothing here
/// normalizes, lowercases, or transcodes: NFC normalization is a display and
/// search concern and happens at that boundary only
/// (docs/03-MACOS.md#names-case-and-paths).
#[derive(Default, Clone, PartialEq, Eq)]
pub struct NameBlob {
    bytes: Vec<u8>,
}

impl NameBlob {
    /// Bytes of heap this blob occupies.
    ///
    /// `capacity`, not `len`: the question this answers is "how much memory is
    /// this holding", and a `Vec` that doubled to 4 GiB and then had half its
    /// contents dropped is still holding 4 GiB. Reporting `len` would make the
    /// scan-admission budget in `src-tauri` optimistic in exactly the situation
    /// where it must not be.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.bytes.capacity()
    }

    /// An empty blob.
    #[must_use]
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// An empty blob with room for `capacity` bytes.
    ///
    /// The scanner sizes this explicitly from the observed mean name length
    /// rather than letting a `Vec` double its way through gigabytes
    /// (docs/01-ARCHITECTURE.md#memory-budget).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// Appends `name` and returns its reference.
    ///
    /// # Errors
    ///
    /// - [`ArenaError::NameTooLong`] if `name` exceeds [`MAX_NAME_LEN`].
    /// - [`ArenaError::NameOffsetOverflow`] if the blob has grown past
    ///   [`MAX_NAME_OFFSET`].
    pub fn push(&mut self, name: &[u8]) -> Result<NameRef, ArenaError> {
        let len = u16::try_from(name.len()).map_err(|_| ArenaError::NameTooLong { len: name.len() })?;
        let offset =
            u64::try_from(self.bytes.len()).map_err(|_| ArenaError::NameOffsetOverflow { offset: u64::MAX })?;
        let reference = NameRef::new(offset, len)?;
        self.bytes.extend_from_slice(name);
        Ok(reference)
    }

    /// Resolves `reference` against this blob.
    ///
    /// This is **the** accessor. It returns `None` for any reference whose range
    /// is not fully inside the blob, which is what makes a corrupted snapshot a
    /// rejected query instead of an out-of-bounds read.
    #[must_use]
    pub fn bytes(&self, reference: NameRef) -> Option<&[u8]> {
        let start = usize::try_from(reference.offset()).ok()?;
        let end = start.checked_add(usize::from(reference.len()))?;
        self.bytes.get(start..end)
    }

    /// Whether `reference` addresses a range fully inside this blob.
    ///
    /// Used by snapshot validation, which checks every reference before any
    /// node is exposed to a command.
    #[must_use]
    pub fn contains(&self, reference: NameRef) -> bool {
        self.bytes(reference).is_some()
    }

    /// Total bytes stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether no names have been stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Current allocated capacity, in bytes. Reported in the memory profile.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// The whole blob, for snapshot writing.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Rebuilds a blob from snapshot bytes.
    ///
    /// The caller still validates every [`NameRef`] against the result before
    /// exposing a node.
    #[must_use]
    pub const fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Releases unused capacity once the tree is frozen.
    pub fn shrink_to_fit(&mut self) {
        self.bytes.shrink_to_fit();
    }
}

impl fmt::Debug for NameBlob {
    /// Summary only. A derived `Debug` over 1.29 GiB of names is a way to run
    /// out of memory inside a log statement.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NameBlob")
            .field("len", &self.bytes.len())
            .field("capacity", &self.bytes.capacity())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_lossless_bytes_including_invalid_utf8() {
        let mut blob = NameBlob::new();
        let weird: &[u8] = &[0xff, 0xfe, b'a', 0x80];
        let a = blob.push(b"Movies").expect("fits");
        let b = blob.push(weird).expect("fits");
        assert_eq!(blob.bytes(a), Some(&b"Movies"[..]));
        assert_eq!(blob.bytes(b), Some(weird));
    }

    #[test]
    fn packing_survives_the_full_offset_range() {
        let reference = NameRef::new(MAX_NAME_OFFSET, u16::MAX).expect("at the ceiling");
        assert_eq!(reference.offset(), MAX_NAME_OFFSET);
        assert_eq!(reference.len(), u16::MAX);
        assert!(NameRef::new(MAX_NAME_OFFSET + 1, 1).is_err());
    }

    #[test]
    fn out_of_range_reference_is_rejected_rather_than_slicing() {
        let mut blob = NameBlob::new();
        blob.push(b"abc").expect("fits");
        let past_end = NameRef::new(2, 8).expect("packs");
        assert_eq!(blob.bytes(past_end), None);
        assert!(!blob.contains(past_end));
        let absurd = NameRef::new(MAX_NAME_OFFSET, 1).expect("packs");
        assert_eq!(blob.bytes(absurd), None);
    }

    #[test]
    fn oversized_name_is_an_error_not_a_truncation() {
        let mut blob = NameBlob::new();
        let huge = vec![b'x'; MAX_NAME_LEN + 1];
        assert!(matches!(blob.push(&huge), Err(ArenaError::NameTooLong { .. })));
        assert!(blob.is_empty(), "a rejected push must not append");
    }

    #[test]
    fn empty_reference_reads_as_an_empty_name() {
        let blob = NameBlob::new();
        assert_eq!(blob.bytes(NameRef::EMPTY), Some(&b""[..]));
    }
}
