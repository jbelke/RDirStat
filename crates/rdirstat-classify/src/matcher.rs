//! The two matching primitives: a flat byte-keyed lookup table and a tiny
//! byte glob. Both are built once when a configuration is compiled and are
//! **read-only and allocation-free** afterwards, because every lookup happens
//! on the builder thread while a name is hot (docs/08-RUST-PRACTICES.md#no-allocation-in-the-hot-loop).

use core::fmt;

use crate::schema::ConfigError;

/// The stack buffer used to ASCII-fold a suffix or a path component.
///
/// Nothing in a real configuration comes close: the longest default key is
/// under 20 bytes. [`crate::Categorizer::compile`] *rejects* a folded key
/// longer than this, which turns the fallback from "measured heuristic" into a
/// proof — a key that cannot be folded on the stack cannot exist in the table,
/// so skipping the folded probe for an over-long input can never miss a match.
pub(crate) const FOLD_CAPACITY: usize = 32;

/// FNV-1a over a short byte key.
///
/// Keys here are file-name suffixes and path components, so the loop runs a
/// handful of times. A stronger hash would cost more than the collisions it
/// avoids at these sizes.
fn hash(key: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut accumulator = OFFSET;
    for &byte in key {
        accumulator = (accumulator ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    // One avalanche step: FNV's low bits are weak, and we index with them.
    accumulator ^= accumulator >> 29;
    accumulator = accumulator.wrapping_mul(0xff51_afd7_ed55_8ccd);
    accumulator ^ (accumulator >> 32)
}

/// Maps a hash onto a slot without an `as` cast (`cast_possible_truncation` is
/// denied workspace-wide). The table never has more than `u32::MAX` slots.
fn slot_index(hash: u64, mask: usize) -> usize {
    usize::try_from(hash & 0xffff_ffff).unwrap_or(0) & mask
}

/// ASCII-folds `input` into `buffer`, returning the folded bytes.
///
/// Returns `None` when `input` is longer than [`FOLD_CAPACITY`]; see the note
/// there for why that is safe to treat as "no match".
pub(crate) fn fold_ascii<'buffer>(input: &[u8], buffer: &'buffer mut [u8; FOLD_CAPACITY]) -> Option<&'buffer [u8]> {
    let len = input.len();
    if len > FOLD_CAPACITY {
        return None;
    }
    for (slot, byte) in buffer.iter_mut().zip(input) {
        *slot = byte.to_ascii_lowercase();
    }
    buffer.get(..len)
}

#[derive(Clone, Copy)]
struct Slot<V: Copy> {
    offset: u32,
    /// `0` marks an empty slot. Keys are validated non-empty, so this is not
    /// ambiguous.
    len: u16,
    value: V,
}

/// An open-addressed, linear-probing map from a short byte key to a `Copy`
/// value.
///
/// Chosen over `HashMap<Box<[u8]>, _>` for three reasons that matter at 69M
/// entries: `SipHash` is overkill for a three-byte extension, the keys live in
/// one contiguous blob instead of one allocation each, and a probe touches at
/// most two cache lines. Load factor is kept at or below 1/2, which is also
/// what guarantees the probe loop terminates.
#[derive(Clone)]
pub(crate) struct Table<V: Copy + Default> {
    blob: Vec<u8>,
    slots: Vec<Slot<V>>,
    mask: usize,
    len: usize,
    /// Bit `n` set means the table holds at least one key of length `n`.
    ///
    /// Keys are at most [`FOLD_CAPACITY`] bytes, so 33 bits suffice. This is a
    /// one-shift rejection for the common case — the exact-case table holds a
    /// handful of keys, and without it every name pays a full hash and probe
    /// to learn that.
    lengths: u64,
}

impl<V: Copy + Default> fmt::Debug for Table<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Table")
            .field("entries", &self.len)
            .field("slots", &self.slots.len())
            .field("key_bytes", &self.blob.len())
            .finish_non_exhaustive()
    }
}

impl<V: Copy + Default> Table<V> {
    /// An empty table. Every lookup misses.
    pub(crate) fn empty() -> Self {
        Self {
            blob: Vec::new(),
            slots: Vec::new(),
            mask: 0,
            len: 0,
            lengths: 0,
        }
    }

    /// Builds a table from already-deduplicated entries.
    ///
    /// # Errors
    ///
    /// [`ConfigError::EmptyKey`] for an empty key, [`ConfigError::KeyTooLong`]
    /// for a key that will not fit the stack fold buffer, and
    /// [`ConfigError::TableOverflow`] if the key blob exceeds `u32::MAX`.
    pub(crate) fn build(entries: &[(Vec<u8>, V)]) -> Result<Self, ConfigError> {
        if entries.is_empty() {
            return Ok(Self::empty());
        }
        let capacity = entries.len().saturating_mul(2).next_power_of_two().max(8);
        let mut table = Self {
            blob: Vec::with_capacity(entries.iter().map(|(key, _)| key.len()).sum()),
            slots: vec![
                Slot {
                    offset: 0,
                    len: 0,
                    value: V::default()
                };
                capacity
            ],
            mask: capacity - 1,
            len: 0,
            lengths: 0,
        };

        for (key, value) in entries {
            if key.is_empty() {
                return Err(ConfigError::EmptyKey);
            }
            if key.len() > FOLD_CAPACITY {
                return Err(ConfigError::KeyTooLong {
                    key: String::from_utf8_lossy(key).into_owned(),
                    limit: FOLD_CAPACITY,
                });
            }
            let offset = u32::try_from(table.blob.len()).map_err(|_| ConfigError::TableOverflow)?;
            let len = u16::try_from(key.len()).map_err(|_| ConfigError::TableOverflow)?;
            table.lengths |= 1u64 << u32::from(len);
            table.blob.extend_from_slice(key);

            let mut index = slot_index(hash(key), table.mask);
            loop {
                let Some(slot) = table.slots.get_mut(index) else {
                    return Err(ConfigError::TableOverflow);
                };
                if slot.len == 0 {
                    *slot = Slot {
                        offset,
                        len,
                        value: *value,
                    };
                    table.len += 1;
                    break;
                }
                index = (index + 1) & table.mask;
            }
        }
        Ok(table)
    }

    /// Looks a key up. Allocation-free; `key` borrows the caller's bytes.
    pub(crate) fn get(&self, key: &[u8]) -> Option<V> {
        // Rejects most probes before a single byte is hashed.
        if key.len() > FOLD_CAPACITY || self.lengths >> u32::try_from(key.len()).unwrap_or(63) & 1 == 0 {
            return None;
        }
        let mut index = slot_index(hash(key), self.mask);
        loop {
            let slot = self.slots.get(index)?;
            if slot.len == 0 {
                return None;
            }
            if usize::from(slot.len) == key.len() {
                let start = usize::try_from(slot.offset).unwrap_or(usize::MAX);
                if self.blob.get(start..start.saturating_add(key.len())) == Some(key) {
                    return Some(slot.value);
                }
            }
            index = (index + 1) & self.mask;
        }
    }

    /// The number of keys in the table.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }
}

/// Which entry kinds a basename pattern may match.
///
/// A directory named `node_modules` is a dependency tree; a *file* named
/// `node_modules` is not (docs/04-CLASSIFICATION.md#context-tagging).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GlobScope {
    /// Matches files and directories alike.
    #[default]
    Any,
    /// Matches non-directory entries only.
    Files,
    /// Matches directories only.
    Directories,
}

impl GlobScope {
    /// Whether this scope admits an entry with the given directory-ness.
    #[must_use]
    pub const fn admits(self, is_directory: bool) -> bool {
        match self {
            Self::Any => true,
            Self::Files => !is_directory,
            Self::Directories => is_directory,
        }
    }

    pub(crate) const fn key(self) -> u8 {
        match self {
            Self::Any => 0,
            Self::Files => 1,
            Self::Directories => 2,
        }
    }
}

/// A compiled basename glob: `*` (any run) and `?` (any one byte). Nothing
/// else — bracket classes and brace expansion are not worth the ambiguity in a
/// list this small, and `[` is a legal file-name byte on macOS.
#[derive(Clone, Debug)]
pub(crate) struct ByteGlob {
    pattern: Box<[u8]>,
    case_sensitive: bool,
    scope: GlobScope,
}

impl ByteGlob {
    /// Compiles a pattern. A case-insensitive pattern is folded once, here.
    pub(crate) fn new(pattern: &[u8], case_sensitive: bool, scope: GlobScope) -> Self {
        let stored: Box<[u8]> = if case_sensitive {
            pattern.into()
        } else {
            pattern.iter().map(u8::to_ascii_lowercase).collect()
        };
        Self {
            pattern: stored,
            case_sensitive,
            scope,
        }
    }

    pub(crate) const fn scope(&self) -> GlobScope {
        self.scope
    }

    pub(crate) fn pattern(&self) -> &[u8] {
        &self.pattern
    }

    /// The first pattern byte, when it is a literal that every match must
    /// start with. `None` for a pattern that opens with `*` or `?`, or for the
    /// empty pattern (which validation rejects anyway).
    fn anchor(&self) -> Option<u8> {
        match self.pattern.first().copied() {
            Some(b'*' | b'?') | None => None,
            Some(byte) => Some(byte),
        }
    }

    /// Whether `name` matches. Allocation-free, including the folded case.
    pub(crate) fn matches(&self, name: &[u8]) -> bool {
        let pattern = &self.pattern;
        let fold = !self.case_sensitive;
        let (mut p, mut n) = (0usize, 0usize);
        let (mut star, mut retry) = (usize::MAX, 0usize);

        while n < name.len() {
            let candidate = pattern.get(p).copied();
            let subject = name.get(n).copied().unwrap_or(0);
            let subject = if fold { subject.to_ascii_lowercase() } else { subject };
            match candidate {
                Some(b'?') => {
                    p += 1;
                    n += 1;
                }
                Some(b'*') => {
                    star = p;
                    p += 1;
                    retry = n;
                }
                Some(byte) if byte == subject => {
                    p += 1;
                    n += 1;
                }
                _ if star != usize::MAX => {
                    p = star + 1;
                    retry += 1;
                    n = retry;
                }
                _ => return false,
            }
        }
        while pattern.get(p) == Some(&b'*') {
            p += 1;
        }
        p == pattern.len()
    }
}

/// The ordered basename-glob list, indexed by the name's **first byte**.
///
/// A linear walk over every pattern is what a naive implementation does, and
/// it costs ~140 ns per unmatched name — which is most names, because the glob
/// list is precisely the fallback for everything the suffix maps missed. Since
/// a pattern that starts with a literal byte can only match a name starting
/// with that byte (either case, when folded), one 256-entry index reduces the
/// walk to the handful of patterns that could possibly match, plus the tiny
/// list of patterns that open with a wildcard.
///
/// Declaration order is preserved exactly: both lists are ascending in
/// declaration index and are merged, so "first declared wins" still holds.
pub(crate) struct GlobIndex<V: Copy> {
    globs: Vec<(ByteGlob, V)>,
    /// 257 prefix sums into `items`, one per possible first byte.
    starts: Vec<u32>,
    items: Vec<u16>,
    /// Patterns that open with `*` or `?`, so any first byte may match.
    anywhere: Vec<u16>,
}

impl<V: Copy> fmt::Debug for GlobIndex<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobIndex")
            .field("globs", &self.globs.len())
            .field("wildcard_leading", &self.anywhere.len())
            .finish_non_exhaustive()
    }
}

impl<V: Copy> Clone for GlobIndex<V> {
    fn clone(&self) -> Self {
        Self {
            globs: self.globs.clone(),
            starts: self.starts.clone(),
            items: self.items.clone(),
            anywhere: self.anywhere.clone(),
        }
    }
}

impl<V: Copy> GlobIndex<V> {
    /// Builds the index from patterns in declaration order.
    ///
    /// # Errors
    ///
    /// [`ConfigError::TableOverflow`] if more than `u16::MAX` patterns are
    /// declared. A "deliberately small ordered list" is nowhere near that.
    pub(crate) fn build(globs: Vec<(ByteGlob, V)>) -> Result<Self, ConfigError> {
        u16::try_from(globs.len()).map_err(|_| ConfigError::TableOverflow)?;

        let mut counts = [0u32; 256];
        let mut anywhere = Vec::new();
        for (index, (glob, _)) in globs.iter().enumerate() {
            let Ok(index) = u16::try_from(index) else {
                return Err(ConfigError::TableOverflow);
            };
            match glob.anchor() {
                None => anywhere.push(index),
                Some(byte) => {
                    for anchor in anchors(byte, glob.case_sensitive) {
                        if let Some(slot) = counts.get_mut(usize::from(anchor)) {
                            *slot += 1;
                        }
                    }
                }
            }
        }

        let mut starts = vec![0u32; 257];
        let mut running = 0u32;
        for (slot, count) in starts.iter_mut().zip(counts).take(256) {
            *slot = running;
            running += count;
        }
        if let Some(last) = starts.last_mut() {
            *last = running;
        }

        let mut cursor = starts.clone();
        let mut items = vec![0u16; usize::try_from(running).unwrap_or(0)];
        for (index, (glob, _)) in globs.iter().enumerate() {
            let Ok(index) = u16::try_from(index) else {
                return Err(ConfigError::TableOverflow);
            };
            let Some(byte) = glob.anchor() else { continue };
            for anchor in anchors(byte, glob.case_sensitive) {
                let Some(position) = cursor.get_mut(usize::from(anchor)) else {
                    continue;
                };
                if let Some(slot) = items.get_mut(usize::try_from(*position).unwrap_or(usize::MAX)) {
                    *slot = index;
                }
                *position += 1;
            }
        }

        Ok(Self {
            globs,
            starts,
            items,
            anywhere,
        })
    }

    /// The number of compiled patterns.
    pub(crate) const fn len(&self) -> usize {
        self.globs.len()
    }

    /// The first declared pattern that matches, in declaration order.
    /// Allocation-free.
    pub(crate) fn first_match(&self, name: &[u8], is_directory: bool) -> Option<V> {
        let bucket = match name.first() {
            Some(&first) => {
                let start = usize::try_from(*self.starts.get(usize::from(first))?).unwrap_or(0);
                let end = usize::try_from(*self.starts.get(usize::from(first) + 1)?).unwrap_or(0);
                self.items.get(start..end)?
            }
            // An empty name can only be matched by a wildcard-leading pattern.
            None => &[],
        };

        let (mut left, mut right) = (0usize, 0usize);
        loop {
            let candidate = match (bucket.get(left).copied(), self.anywhere.get(right).copied()) {
                (None, None) => return None,
                (Some(anchored), None) => {
                    left += 1;
                    anchored
                }
                (None, Some(wildcard)) => {
                    right += 1;
                    wildcard
                }
                (Some(anchored), Some(wildcard)) => {
                    if anchored <= wildcard {
                        left += 1;
                        anchored
                    } else {
                        right += 1;
                        wildcard
                    }
                }
            };
            let (glob, value) = self.globs.get(usize::from(candidate))?;
            if glob.scope().admits(is_directory) && glob.matches(name) {
                return Some(*value);
            }
        }
    }
}

/// The first bytes a name may start with for this pattern to match.
fn anchors(byte: u8, case_sensitive: bool) -> impl Iterator<Item = u8> {
    let upper = byte.to_ascii_uppercase();
    let second = if case_sensitive || upper == byte {
        None
    } else {
        Some(upper)
    };
    core::iter::once(byte).chain(second)
}

#[cfg(test)]
mod tests {
    use super::{ByteGlob, FOLD_CAPACITY, GlobIndex, GlobScope, Table, fold_ascii};

    fn index(patterns: &[(&str, bool, GlobScope, u8)]) -> GlobIndex<u8> {
        let globs = patterns
            .iter()
            .map(|(pattern, case_sensitive, scope, value)| {
                (ByteGlob::new(pattern.as_bytes(), *case_sensitive, *scope), *value)
            })
            .collect();
        GlobIndex::build(globs).expect("index builds")
    }

    #[test]
    fn glob_index_preserves_declaration_order() {
        // Two patterns that both match: the first declared must win, whether
        // the winner is the anchored one or the wildcard-leading one.
        let anchored_first = index(&[
            ("Makefile", false, GlobScope::Any, 1),
            ("*file", false, GlobScope::Any, 2),
        ]);
        assert_eq!(anchored_first.first_match(b"Makefile", false), Some(1));

        let wildcard_first = index(&[
            ("*file", false, GlobScope::Any, 2),
            ("Makefile", false, GlobScope::Any, 1),
        ]);
        assert_eq!(wildcard_first.first_match(b"Makefile", false), Some(2));
    }

    #[test]
    fn glob_index_respects_scope_and_keeps_looking() {
        // The first candidate matches by name but is scoped to directories, so
        // the walk must continue to the second rather than give up.
        let scoped = index(&[
            ("thing", false, GlobScope::Directories, 1),
            ("thing", false, GlobScope::Files, 2),
        ]);
        assert_eq!(scoped.first_match(b"thing", true), Some(1));
        assert_eq!(scoped.first_match(b"thing", false), Some(2));
    }

    #[test]
    fn glob_index_folds_the_anchor_byte() {
        let folded = index(&[("makefile", false, GlobScope::Any, 7)]);
        assert_eq!(folded.first_match(b"Makefile", false), Some(7));
        assert_eq!(folded.first_match(b"makefile", false), Some(7));

        let exact = index(&[(".DS_Store", true, GlobScope::Any, 9)]);
        assert_eq!(exact.first_match(b".DS_Store", false), Some(9));
        assert_eq!(exact.first_match(b".ds_store", false), None);
    }

    #[test]
    fn glob_index_handles_the_empty_name() {
        let with_wildcard = index(&[("*", true, GlobScope::Any, 1)]);
        assert_eq!(with_wildcard.first_match(b"", false), Some(1));
        let anchored = index(&[("a", true, GlobScope::Any, 1)]);
        assert_eq!(anchored.first_match(b"", false), None);
        let empty: GlobIndex<u8> = GlobIndex::build(Vec::new()).expect("empty index builds");
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.first_match(b"anything", false), None);
    }

    #[test]
    fn glob_index_matches_the_same_set_as_a_linear_walk() {
        let patterns: &[(&str, bool, GlobScope, u8)] = &[
            (".DS_Store", true, GlobScope::Files, 1),
            ("._*", true, GlobScope::Any, 2),
            ("node_modules", false, GlobScope::Directories, 3),
            ("Makefile", false, GlobScope::Any, 4),
            ("*.tmp", false, GlobScope::Any, 5),
            ("Icon\r", true, GlobScope::Files, 6),
        ];
        let index = index(patterns);
        let names: &[&[u8]] = &[
            b".DS_Store",
            b".ds_store",
            b"._twin",
            b"node_modules",
            b"MAKEFILE",
            b"scratch.tmp",
            b"Icon\r",
            b"",
            b"nothing-here",
            b"\xff\xfe",
        ];
        for is_directory in [false, true] {
            for name in names {
                let linear = patterns
                    .iter()
                    .find(|(pattern, case_sensitive, scope, _)| {
                        scope.admits(is_directory)
                            && ByteGlob::new(pattern.as_bytes(), *case_sensitive, *scope).matches(name)
                    })
                    .map(|(_, _, _, value)| *value);
                assert_eq!(
                    index.first_match(name, is_directory),
                    linear,
                    "{:?} dir={is_directory}",
                    String::from_utf8_lossy(name)
                );
            }
        }
    }

    fn table(entries: &[(&str, u8)]) -> Table<u8> {
        let owned: Vec<(Vec<u8>, u8)> = entries
            .iter()
            .map(|(key, value)| ((*key).as_bytes().to_vec(), *value))
            .collect();
        Table::build(&owned).expect("table builds")
    }

    #[test]
    fn table_round_trips_every_key() {
        let entries: Vec<(String, u8)> = (0..200u8).map(|i| (format!("key{i}"), i)).collect::<Vec<_>>();
        let owned: Vec<(Vec<u8>, u8)> = entries.iter().map(|(k, v)| (k.as_bytes().to_vec(), *v)).collect();
        let built = Table::build(&owned).expect("table builds");
        assert_eq!(built.len(), 200);
        for (key, value) in &entries {
            assert_eq!(built.get(key.as_bytes()), Some(*value), "missing {key}");
        }
        assert_eq!(built.get(b"absent"), None);
    }

    #[test]
    fn empty_table_misses_without_looping() {
        let built: Table<u8> = Table::empty();
        assert_eq!(built.get(b"anything"), None);
        assert_eq!(built.len(), 0);
    }

    #[test]
    fn table_distinguishes_prefixes() {
        let built = table(&[("tar", 1), ("tar.gz", 2), ("gz", 3)]);
        assert_eq!(built.get(b"tar"), Some(1));
        assert_eq!(built.get(b"tar.gz"), Some(2));
        assert_eq!(built.get(b"gz"), Some(3));
        assert_eq!(built.get(b"ta"), None);
    }

    #[test]
    fn table_rejects_a_key_longer_than_the_fold_buffer() {
        let key = vec![b'a'; FOLD_CAPACITY + 1];
        assert!(Table::build(&[(key, 1u8)]).is_err());
    }

    #[test]
    fn table_handles_non_utf8_keys() {
        let built = Table::build(&[(vec![0xff, 0xfe], 7u8)]).expect("table builds");
        assert_eq!(built.get(&[0xff, 0xfe]), Some(7));
    }

    #[test]
    fn fold_is_ascii_only() {
        let mut buffer = [0u8; FOLD_CAPACITY];
        assert_eq!(fold_ascii(b"JPEG", &mut buffer), Some(&b"jpeg"[..]));
        // U+00C4 in UTF-8 is C3 84; ASCII folding must leave both bytes alone.
        let mut other = [0u8; FOLD_CAPACITY];
        assert_eq!(fold_ascii(&[0xc3, 0x84], &mut other), Some(&[0xc3, 0x84][..]));
    }

    #[test]
    fn fold_refuses_over_long_input() {
        let mut buffer = [0u8; FOLD_CAPACITY];
        assert_eq!(fold_ascii(&[b'a'; FOLD_CAPACITY + 1], &mut buffer), None);
    }

    #[test]
    fn glob_literal_and_wildcards() {
        let exact = ByteGlob::new(b".DS_Store", true, GlobScope::Files);
        assert!(exact.matches(b".DS_Store"));
        assert!(!exact.matches(b".ds_store"));
        assert!(!exact.matches(b"x.DS_Store"));

        let prefix = ByteGlob::new(b"._*", true, GlobScope::Any);
        assert!(prefix.matches(b"._photo.jpg"));
        assert!(prefix.matches(b"._"));
        assert!(!prefix.matches(b"_photo.jpg"));

        let single = ByteGlob::new(b"log.?", true, GlobScope::Files);
        assert!(single.matches(b"log.1"));
        assert!(!single.matches(b"log.12"));
    }

    #[test]
    fn glob_folds_when_case_insensitive() {
        let folded = ByteGlob::new(b"Makefile", false, GlobScope::Files);
        assert!(folded.matches(b"makefile"));
        assert!(folded.matches(b"MAKEFILE"));
        assert!(!folded.matches(b"makefile.in"));
    }

    #[test]
    fn glob_backtracks() {
        let tricky = ByteGlob::new(b"*a*b", true, GlobScope::Any);
        assert!(tricky.matches(b"xxaxxb"));
        assert!(tricky.matches(b"ab"));
        assert!(!tricky.matches(b"ba"));
        assert!(ByteGlob::new(b"*", true, GlobScope::Any).matches(b""));
        assert!(!ByteGlob::new(b"a*", true, GlobScope::Any).matches(b""));
    }

    #[test]
    fn glob_scope_admits() {
        assert!(GlobScope::Any.admits(true) && GlobScope::Any.admits(false));
        assert!(GlobScope::Directories.admits(true) && !GlobScope::Directories.admits(false));
        assert!(!GlobScope::Files.admits(true) && GlobScope::Files.admits(false));
    }
}
