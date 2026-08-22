use crate::core::wallet::{NonceScope, NonceState};
use async_trait::async_trait;

/// A stored value together with its compare-and-swap version. Version `0` means
/// absent, so [`Versioned::default`] is the "not yet stored" state.
#[derive(Debug, Clone, Default)]
pub struct Versioned<T> {
    pub value: T,
    pub version: u64,
}

/// Durable state behind the executor, accessed via compare-and-swap so the same
/// [`NonceManager`](super::NonceManager) works single-process and distributed —
/// only the store changes. Phase 1 holds the per-scope nonce state; idempotency
/// map, tx handles, and pending intents are added by the tasks that consume them.
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
}

/// Variants grow with the store adapters (the in-memory store never errors; a
/// durable backend adds I/O failures later).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateStoreError {}
