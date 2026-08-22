use crate::core::wallet::{NonceScope, NonceState, TxHandle};
use alloy_primitives::Address;
use async_trait::async_trait;

/// A stored value together with its compare-and-swap version. Version `0` means
/// absent, so [`Versioned::default`] is the "not yet stored" state.
#[derive(Debug, Clone, Default)]
pub struct Versioned<T> {
    pub value: T,
    pub version: u64,
}

/// Durable state behind the executor, accessed via compare-and-swap so one
/// [`NonceManager`](super::NonceManager) serves both single-process and distributed
/// deployments — only the store changes. Phase 1 holds per-scope nonce state.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Load a scope's nonce state with its version (absent → `Versioned::default`).
    async fn load_nonce_state(
        &self,
        scope: NonceScope,
    ) -> Result<Versioned<NonceState>, StateStoreError>;

    /// Store `state` iff the current version equals `expected_version`, bumping the
    /// version. Returns `true` on success, `false` on a version conflict (caller retries).
    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
    ) -> Result<bool, StateStoreError>;

    /// Persist a handle before its broadcast (persist-before-broadcast, so a crash
    /// is recoverable). Overwrites by [`id`](TxHandle::id).
    async fn put_handle(&self, handle: &TxHandle) -> Result<(), StateStoreError>;

    /// Non-terminal handles for `account`, for the executor to recover/track. The
    /// crash-recovery read: on boot these are the in-flight txs to rebroadcast.
    async fn pending_handles(&self, account: Address) -> Result<Vec<TxHandle>, StateStoreError>;
}

/// Variants grow with the store adapters (the in-memory store never errors; a
/// durable backend adds I/O failures later).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateStoreError {}
