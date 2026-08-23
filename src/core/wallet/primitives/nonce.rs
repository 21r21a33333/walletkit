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

/// The key a [`NonceState`] is stored under. Keyed by account (EOA); a 4337 2D-nonce
/// lane can be added here without changing the manager.
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
/// Only [`SINGLE_WRITER`](FenceToken::SINGLE_WRITER) is used today; a distributed lease
/// issuer mints real tokens later with no trait change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FenceToken(u64);

impl FenceToken {
    /// The sole token in single-writer mode — every write carries it, so the
    /// reject-if-lower check is always satisfied (a no-op) until a lease issuer exists.
    pub const SINGLE_WRITER: FenceToken = FenceToken(0);
}

#[cfg(feature = "postgres")]
impl FenceToken {
    /// The raw token value. Postgres stores the fence as a `BIGINT` column, so the adapter
    /// needs the scalar; redb serializes the token directly and needs no accessor. (The
    /// inverse `from_u64` is added when a consumer reconstructs a token from storage — the
    /// CAS compares in `i64` space and never does, so it isn't needed yet.)
    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
impl FenceToken {
    /// Test-only: a token above `SINGLE_WRITER` to exercise the reject-if-lower path.
    pub(crate) fn for_test(n: u64) -> Self {
        FenceToken(n)
    }
}
