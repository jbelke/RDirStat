//! A deterministic 256-bit digest over a compiled category configuration.
//!
//! This is **not** a cryptographic hash. It exists for one job: deciding
//! whether two scans were classified by the same configuration, so an old
//! snapshot's `CategoryId` values stay interpretable
//! (docs/04-CLASSIFICATION.md#context-tagging). It must therefore be stable
//! across processes and architectures, which rules out `DefaultHasher`
//! (documented as unstable across releases) and `HashMap` iteration order.
//!
//! Four independent FNV-1a-style lanes are mixed byte at a time and folded
//! together at the end. Field boundaries are separated explicitly so that
//! `["ab", "c"]` and `["a", "bc"]` produce different digests.

/// Distinct offset bases, so the four lanes do not agree on the empty input.
const OFFSETS: [u64; 4] = [
    0xcbf2_9ce4_8422_2325,
    0x9e37_79b9_7f4a_7c15,
    0xff51_afd7_ed55_8ccd,
    0xc4ce_b9fe_1a85_ec53,
];

/// Distinct odd multipliers, so the lanes diverge under the same input.
const PRIMES: [u64; 4] = [
    0x0000_0100_0000_01b3,
    0x9e37_79b1_85eb_ca87,
    0xc2b2_ae3d_27d4_eb4f,
    0x1656_67b1_9e37_79f9,
];

/// Distinct rotations, so a lane's high bits reach its low bits differently.
const ROTATIONS: [u32; 4] = [13, 20, 27, 34];

/// A streaming, order-sensitive 256-bit digest.
#[derive(Clone)]
pub(crate) struct Digest256 {
    lanes: [u64; 4],
    written: u64,
}

impl core::fmt::Debug for Digest256 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Digest256")
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

impl Digest256 {
    /// A digest with nothing absorbed yet.
    pub(crate) const fn new() -> Self {
        Self {
            lanes: OFFSETS,
            written: 0,
        }
    }

    /// Absorbs raw bytes.
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let value = u64::from(byte);
            for ((lane, prime), rotation) in self.lanes.iter_mut().zip(PRIMES).zip(ROTATIONS) {
                *lane = (*lane ^ value).wrapping_mul(prime).rotate_left(rotation);
            }
        }
        self.written = self
            .written
            .wrapping_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    /// Absorbs a length-prefixed field, so concatenation is not ambiguous.
    pub(crate) fn field(&mut self, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.update(&len.to_le_bytes());
        self.update(bytes);
    }

    /// Absorbs a single byte as its own field.
    pub(crate) fn byte(&mut self, value: u8) {
        self.update(&[0xff, value]);
    }

    /// Folds the lanes together and emits the digest.
    pub(crate) fn finish(&self) -> [u8; 32] {
        let mut mixed = self.lanes;
        // Two folding rounds so a change in any lane reaches every output byte.
        for round in 0..2usize {
            let previous = mixed;
            for (lane, slot) in mixed.iter_mut().enumerate() {
                let neighbour = previous[(lane + 1) & 3];
                *slot = (*slot ^ neighbour.rotate_left(ROTATIONS[(lane + round) & 3]))
                    .wrapping_add(self.written)
                    .wrapping_mul(PRIMES[(lane + round) & 3]);
                *slot ^= *slot >> 31;
            }
        }

        let mut out = [0u8; 32];
        for (chunk, lane) in out.chunks_exact_mut(8).zip(mixed) {
            chunk.copy_from_slice(&lane.to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Digest256;

    fn digest_of(fields: &[&[u8]]) -> [u8; 32] {
        let mut digest = Digest256::new();
        for field in fields {
            digest.field(field);
        }
        digest.finish()
    }

    #[test]
    fn empty_digest_is_stable_within_a_build() {
        assert_eq!(Digest256::new().finish(), Digest256::new().finish());
    }

    #[test]
    fn field_boundaries_are_not_ambiguous() {
        assert_ne!(digest_of(&[b"ab", b"c"]), digest_of(&[b"a", b"bc"]));
    }

    #[test]
    fn order_matters() {
        assert_ne!(digest_of(&[b"a", b"b"]), digest_of(&[b"b", b"a"]));
    }

    #[test]
    fn one_bit_changes_the_whole_digest() {
        let left = digest_of(&[b"video"]);
        let right = digest_of(&[b"videp"]);
        assert_ne!(left, right);
        let differing = left.iter().zip(right).filter(|(a, b)| **a != *b).count();
        assert!(
            differing > 16,
            "avalanche too weak: only {differing} of 32 bytes changed"
        );
    }

    #[test]
    fn byte_fields_are_absorbed() {
        let mut with_zero = Digest256::new();
        with_zero.byte(0);
        let mut with_one = Digest256::new();
        with_one.byte(1);
        assert_ne!(with_zero.finish(), with_one.finish());
    }
}
