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

/// The lane a nonce sequence belongs to. Phase 1 is EOA-only; ERC-4337 2D nonces
/// add a `Key(U192)` lane in Phase 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NonceLane {
    #[default]
    Eoa,
}

/// The key a [`NonceState`] is stored under — `(account, lane)` — so distributed
/// stores and 2D lanes are drop-in without changing the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonceScope {
    pub account: Address,
    pub lane: NonceLane,
}

impl NonceScope {
    pub fn eoa(account: Address) -> Self {
        Self {
            account,
            lane: NonceLane::Eoa,
        }
    }
}
