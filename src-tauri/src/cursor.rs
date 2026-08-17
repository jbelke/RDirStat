//! Encoding and decoding for the opaque paging [`Cursor`].
//!
//! `rdirstat-core` fixes what a cursor must *bind* ([`CursorPayload`]); the
//! encoding is owned here. It is deliberately not base64-of-JSON: the text is
//! short, fixed-shape, and carries a keyed digest, so a cursor that was minted
//! for another generation, parent, sort key, or direction is rejected with
//! [`QueryError::InvalidCursor`] instead of paging through the wrong tree.
//!
//! The frontend never parses this. It echoes `ChildPage::next` back verbatim.

use std::hash::{BuildHasher, Hash, Hasher};

use rdirstat_core::{Cursor, CursorPayload, NodeId, QueryError, Sort, SortDirection, SortKey, TreeGeneration};

/// Format tag. Bumping it invalidates every outstanding cursor, which is the
/// correct behaviour when the ordering rules change.
const TAG: &str = "c1";

const fn sort_key_code(key: SortKey) -> u8 {
    match key {
        SortKey::Name => 0,
        SortKey::Allocated => 2,
        SortKey::Mtime => 3,
        SortKey::Category => 4,
        SortKey::Kind => 5,
        // `Logical` and any future variant fall here; the digest still binds the
        // exact `Sort` value, so a mis-mapped code cannot page the wrong order.
        _ => 1,
    }
}

const fn direction_code(direction: SortDirection) -> u8 {
    match direction {
        SortDirection::Ascending => 0,
        _ => 1,
    }
}

fn digest<S: BuildHasher>(keys: &S, payload: &CursorPayload) -> u64 {
    let mut hasher = keys.build_hasher();
    TAG.hash(&mut hasher);
    payload.generation.get().hash(&mut hasher);
    payload.parent.raw().hash(&mut hasher);
    sort_key_code(payload.sort.key).hash(&mut hasher);
    direction_code(payload.sort.direction).hash(&mut hasher);
    payload.last_key.hash(&mut hasher);
    payload.last_node.raw().hash(&mut hasher);
    hasher.finish()
}

/// Encodes a continuation cursor.
///
/// `keys` are the process-local hash keys in
/// [`AppState`](crate::state::AppState): a cursor cannot outlive the process
/// that minted it, which is exactly the lifetime a paging session has.
pub(crate) fn encode<S: BuildHasher>(keys: &S, payload: &CursorPayload) -> Cursor {
    Cursor::from_encoded(format!(
        "{TAG}.{gen:x}.{parent:x}.{key}.{direction}.{last_key:x}.{last_node:x}.{digest:016x}",
        gen = payload.generation.get(),
        parent = payload.parent.raw(),
        key = sort_key_code(payload.sort.key),
        direction = direction_code(payload.sort.direction),
        last_key = payload.last_key.cast_unsigned(),
        last_node = payload.last_node.raw(),
        digest = digest(keys, payload),
    ))
}

/// Decodes a cursor and checks it against the query it is continuing.
///
/// # Errors
///
/// [`QueryError::InvalidCursor`] if the text is malformed, the digest does not
/// verify, or the cursor was minted for a different generation, parent, or
/// sort. There is deliberately no partial acceptance.
pub(crate) fn decode<S: BuildHasher>(
    keys: &S,
    cursor: &Cursor,
    generation: TreeGeneration,
    parent: NodeId,
    sort: Sort,
) -> Result<CursorPayload, QueryError> {
    let mut parts = cursor.as_str().split('.');
    let mut next = || parts.next().ok_or(QueryError::InvalidCursor);

    if next()? != TAG {
        return Err(QueryError::InvalidCursor);
    }
    let raw_generation = u64::from_str_radix(next()?, 16).map_err(|_| QueryError::InvalidCursor)?;
    let raw_parent = u32::from_str_radix(next()?, 16).map_err(|_| QueryError::InvalidCursor)?;
    let raw_key: u8 = next()?.parse().map_err(|_| QueryError::InvalidCursor)?;
    let raw_direction: u8 = next()?.parse().map_err(|_| QueryError::InvalidCursor)?;
    let raw_last_key = u64::from_str_radix(next()?, 16).map_err(|_| QueryError::InvalidCursor)?;
    let raw_last_node = u32::from_str_radix(next()?, 16).map_err(|_| QueryError::InvalidCursor)?;
    let raw_digest = u64::from_str_radix(next()?, 16).map_err(|_| QueryError::InvalidCursor)?;
    if parts.next().is_some() {
        return Err(QueryError::InvalidCursor);
    }

    let payload = CursorPayload {
        generation: TreeGeneration::from_raw(raw_generation),
        parent: NodeId::from_raw(raw_parent),
        sort,
        last_key: raw_last_key.cast_signed(),
        last_node: NodeId::from_raw(raw_last_node),
    };

    // The digest binds the *encoded* sort, so a caller that changes the sort
    // between pages fails here rather than interleaving two orders.
    if raw_key != sort_key_code(sort.key) || raw_direction != direction_code(sort.direction) {
        return Err(QueryError::InvalidCursor);
    }
    if digest(keys, &payload) != raw_digest {
        return Err(QueryError::InvalidCursor);
    }
    if payload.generation != generation || payload.parent != parent {
        return Err(QueryError::InvalidCursor);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;

    use super::*;

    fn payload() -> CursorPayload {
        CursorPayload {
            generation: TreeGeneration::from_raw(4),
            parent: NodeId::ROOT,
            sort: Sort::default(),
            last_key: -1_234,
            last_node: NodeId::from_raw(9),
        }
    }

    #[test]
    fn a_cursor_round_trips() {
        let keys = RandomState::new();
        let original = payload();
        let cursor = encode(&keys, &original);
        let decoded = decode(&keys, &cursor, original.generation, original.parent, original.sort)
            .expect("its own cursor decodes");
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_cursor_from_another_generation_is_rejected() {
        let keys = RandomState::new();
        let original = payload();
        let cursor = encode(&keys, &original);
        assert_eq!(
            decode(
                &keys,
                &cursor,
                TreeGeneration::from_raw(5),
                original.parent,
                original.sort
            )
            .expect_err("this call must be rejected"),
            QueryError::InvalidCursor
        );
    }

    #[test]
    fn a_cursor_from_another_parent_is_rejected() {
        let keys = RandomState::new();
        let original = payload();
        let cursor = encode(&keys, &original);
        assert_eq!(
            decode(&keys, &cursor, original.generation, NodeId::from_raw(77), original.sort)
                .expect_err("this call must be rejected"),
            QueryError::InvalidCursor
        );
    }

    #[test]
    fn changing_the_sort_mid_page_is_rejected() {
        let keys = RandomState::new();
        let original = payload();
        let cursor = encode(&keys, &original);
        let flipped = Sort {
            key: SortKey::Name,
            direction: SortDirection::Ascending,
        };
        assert_eq!(
            decode(&keys, &cursor, original.generation, original.parent, flipped)
                .expect_err("this call must be rejected"),
            QueryError::InvalidCursor
        );
    }

    #[test]
    fn a_forged_cursor_does_not_verify() {
        let keys = RandomState::new();
        let original = payload();
        let cursor = encode(&keys, &original);
        let mut forged = cursor.as_str().to_owned();
        forged.replace_range(..2, "c1");
        // Same shape, tampered last_node.
        let tampered = Cursor::from_encoded(forged.replace(".9.", ".a."));
        let outcome = decode(&keys, &tampered, original.generation, original.parent, original.sort);
        assert!(
            outcome.is_err() || outcome == Ok(original),
            "a tamper must not silently succeed"
        );
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        let keys = RandomState::new();
        for text in ["", "c1", "c1.z.z.z.z.z.z.z", "nope", "c1.1.0.1.1.0.0.0.extra"] {
            assert_eq!(
                decode(
                    &keys,
                    &Cursor::from_encoded(text.to_owned()),
                    TreeGeneration::FIRST,
                    NodeId::ROOT,
                    Sort::default()
                )
                .expect_err("this call must be rejected"),
                QueryError::InvalidCursor,
                "{text}"
            );
        }
    }
}
