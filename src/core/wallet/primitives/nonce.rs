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
