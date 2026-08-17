//! The confirmation token that authorizes a Trash request.
//!
//! `trash_preview` shows the user a normalized selection and the bytes it will
//! move; `move_to_trash` refuses to run without a token minted by that preview.
//! The token binds:
//!
//! 1. the [`TreeGeneration`] — so it cannot be replayed against a new tree;
//! 2. the sorted node set — so it cannot be replayed against a *different*
//!    selection than the one the user saw counted;
//! 3. each item's `(dev, ino)` **as observed by the preview** — so an object
//!    swapped between the sheet and the confirm button fails closed;
//! 4. an expiry, so a token left in a stale webview eventually stops working.
//!
//! The keys are a per-process [`RandomState`](std::collections::hash_map::RandomState),
//! which is randomly seeded. A token therefore does not survive a restart, and
//! a token from one run cannot authorize a move in another.

use std::hash::{BuildHasher, Hash, Hasher};

use rdirstat_core::{ActionError, ConfirmationToken, NodeId, TreeGeneration};

/// Format tag.
const TAG: &str = "t1";

/// How long a minted token stays valid. Long enough for a user to read a
/// confirmation sheet, short enough that a forgotten one expires.
pub(crate) const TOKEN_TTL_MS: i64 = 5 * 60 * 1_000;

/// One item's identity as the preview observed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ItemIdentity {
    /// The selected node.
    pub node: NodeId,
    /// `st_dev` from the preview's `lstat`.
    pub device: u64,
    /// `st_ino` from the preview's `lstat`.
    pub inode: u64,
}

fn digest<S: BuildHasher>(keys: &S, generation: TreeGeneration, expires_unix_ms: i64, items: &[ItemIdentity]) -> u64 {
    let mut sorted: Vec<ItemIdentity> = items.to_vec();
    sorted.sort_unstable_by_key(|item| (item.node.raw(), item.device, item.inode));

    let mut hasher = keys.build_hasher();
    TAG.hash(&mut hasher);
    generation.get().hash(&mut hasher);
    expires_unix_ms.hash(&mut hasher);
    sorted.len().hash(&mut hasher);
    for item in &sorted {
        item.node.raw().hash(&mut hasher);
        item.device.hash(&mut hasher);
        item.inode.hash(&mut hasher);
    }
    hasher.finish()
}

/// Mints a token for a normalized selection.
pub(crate) fn mint<S: BuildHasher>(
    keys: &S,
    generation: TreeGeneration,
    now_unix_ms: i64,
    items: &[ItemIdentity],
) -> ConfirmationToken {
    let expires = now_unix_ms.saturating_add(TOKEN_TTL_MS);
    ConfirmationToken::from_encoded(format!(
        "{TAG}.{gen:x}.{expires:x}.{count:x}.{digest:016x}",
        gen = generation.get(),
        expires = expires.cast_unsigned(),
        count = items.len(),
        digest = digest(keys, generation, expires, items),
    ))
}

/// Verifies a token against the selection as re-observed at action time.
///
/// `items` must carry the `(dev, ino)` from a **fresh** `lstat`, not from the
/// scan record: that is what makes "the object changed between the sheet and
/// the confirm" a rejection rather than a silent move of the wrong file.
///
/// # Errors
///
/// [`ActionError::InvalidConfirmation`] for a malformed, expired, mis-bound, or
/// tampered token.
pub(crate) fn verify<S: BuildHasher>(
    keys: &S,
    token: &ConfirmationToken,
    generation: TreeGeneration,
    now_unix_ms: i64,
    items: &[ItemIdentity],
) -> Result<(), ActionError> {
    let mut parts = token.as_str().split('.');
    let mut next = || parts.next().ok_or(ActionError::InvalidConfirmation);

    if next()? != TAG {
        return Err(ActionError::InvalidConfirmation);
    }
    let raw_generation = u64::from_str_radix(next()?, 16).map_err(|_| ActionError::InvalidConfirmation)?;
    let raw_expires = u64::from_str_radix(next()?, 16).map_err(|_| ActionError::InvalidConfirmation)?;
    let raw_count = usize::from_str_radix(next()?, 16).map_err(|_| ActionError::InvalidConfirmation)?;
    let raw_digest = u64::from_str_radix(next()?, 16).map_err(|_| ActionError::InvalidConfirmation)?;
    if parts.next().is_some() {
        return Err(ActionError::InvalidConfirmation);
    }

    let expires = raw_expires.cast_signed();
    if raw_generation != generation.get() || raw_count != items.len() || now_unix_ms > expires {
        return Err(ActionError::InvalidConfirmation);
    }
    if digest(keys, generation, expires, items) != raw_digest {
        return Err(ActionError::InvalidConfirmation);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::RandomState;

    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn items() -> Vec<ItemIdentity> {
        vec![
            ItemIdentity {
                node: NodeId::from_raw(3),
                device: 16_777_234,
                inode: 42,
            },
            ItemIdentity {
                node: NodeId::from_raw(7),
                device: 16_777_234,
                inode: 99,
            },
        ]
    }

    #[test]
    fn a_fresh_token_verifies_against_its_own_selection() {
        let keys = RandomState::new();
        let generation = TreeGeneration::from_raw(2);
        let token = mint(&keys, generation, NOW, &items());
        verify(&keys, &token, generation, NOW + 1_000, &items()).expect("verifies");
    }

    #[test]
    fn selection_order_does_not_matter() {
        let keys = RandomState::new();
        let generation = TreeGeneration::from_raw(2);
        let token = mint(&keys, generation, NOW, &items());
        let mut reversed = items();
        reversed.reverse();
        verify(&keys, &token, generation, NOW, &reversed).expect("the set is what is bound, not the order");
    }

    #[test]
    fn a_changed_inode_invalidates_the_token() {
        let keys = RandomState::new();
        let generation = TreeGeneration::from_raw(2);
        let token = mint(&keys, generation, NOW, &items());
        let mut swapped = items();
        swapped[0].inode = 43;
        assert_eq!(
            verify(&keys, &token, generation, NOW, &swapped).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn a_token_from_another_generation_is_rejected() {
        let keys = RandomState::new();
        let token = mint(&keys, TreeGeneration::from_raw(2), NOW, &items());
        assert_eq!(
            verify(&keys, &token, TreeGeneration::from_raw(3), NOW, &items()).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn adding_an_item_after_the_preview_is_rejected() {
        let keys = RandomState::new();
        let generation = TreeGeneration::from_raw(2);
        let token = mint(&keys, generation, NOW, &items());
        let mut extra = items();
        extra.push(ItemIdentity {
            node: NodeId::from_raw(11),
            device: 16_777_234,
            inode: 100,
        });
        assert_eq!(
            verify(&keys, &token, generation, NOW, &extra).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let keys = RandomState::new();
        let generation = TreeGeneration::from_raw(2);
        let token = mint(&keys, generation, NOW, &items());
        assert_eq!(
            verify(&keys, &token, generation, NOW + TOKEN_TTL_MS + 1, &items())
                .expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn a_token_from_another_process_does_not_verify() {
        let generation = TreeGeneration::from_raw(2);
        let token = mint(&RandomState::new(), generation, NOW, &items());
        // A different `RandomState` stands in for a different process.
        assert_eq!(
            verify(&RandomState::new(), &token, generation, NOW, &items()).expect_err("this call must be rejected"),
            ActionError::InvalidConfirmation
        );
    }

    #[test]
    fn garbage_is_rejected_rather_than_panicking() {
        let keys = RandomState::new();
        for text in ["", "t1", "t1.z.z.z.z", "t2.1.1.1.1", "t1.1.1.1.1.1"] {
            assert_eq!(
                verify(
                    &keys,
                    &ConfirmationToken::from_encoded(text.to_owned()),
                    TreeGeneration::FIRST,
                    NOW,
                    &items()
                )
                .expect_err("this call must be rejected"),
                ActionError::InvalidConfirmation,
                "{text}"
            );
        }
    }
}
