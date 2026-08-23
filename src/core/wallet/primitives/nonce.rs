use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Per-scope nonce allocation state: the next fresh nonce and the set of freed
/// nonces (from released reservations) to recycle lowest-first.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NonceState {
    pub next: u64,
    pub free: BTreeSet<u64>,
}

/// The key a [`NonceState`] is stored under. Phase 1 keys by account (EOA); a 4337
/// 2D-nonce lane is added here in Phase 5 without changing the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonceScope {
    pub account: Address,
}

impl NonceScope {
    pub fn eoa(account: Address) -> Self {
        Self { account }
    }
}

/// Opaque, monotonic ownership token for the single-writer nonce seam. The
/// [`StateStore`](crate::core::deps::StateStore) records the highest token committed per
/// scope and rejects any lower one (fencing enforced at the resource, per Kleppmann).
/// Phase 1 uses [`SINGLE_WRITER`](FenceToken::SINGLE_WRITER) only; a distributed lease
/// issuer mints real tokens later with no trait change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FenceToken(u64);

impl FenceToken {
    /// The sole token in single-writer mode — every write carries it, so the
    /// reject-if-lower check is always satisfied (a no-op) until a lease issuer exists.
    pub const SINGLE_WRITER: FenceToken = FenceToken(0);
}

#[cfg(test)]
impl FenceToken {
    /// Test-only: a token above `SINGLE_WRITER` to exercise the reject-if-lower path.
    /// (The production `as_u64`/`from_u64` round-trip arrives with the Postgres adapter.)
    pub(crate) fn for_test(n: u64) -> Self {
        FenceToken(n)
    }
}
