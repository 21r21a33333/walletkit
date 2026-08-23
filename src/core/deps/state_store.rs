use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
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
/// deployments — only the store changes.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Load a scope's nonce state with its version (absent → `Versioned::default`).
    async fn load_nonce_state(
        &self,
        scope: NonceScope,
    ) -> Result<Versioned<NonceState>, StateStoreError>;

    /// Store `state` iff the current version equals `expected_version` **and** `fence` is
    /// not below the highest fence committed for `scope`. On success, bump the version and
    /// raise the stored fence to `max(stored, fence)`.
    ///
    /// Two independent guards: the **version** rejects lost updates (`Ok(false)` → the
    /// caller retries); the **fence** rejects a superseded owner
    /// (`Err(`[`StateStoreError::Fenced`]`)` → the caller stops, never retries). In
    /// single-writer mode `fence` is always [`FenceToken::SINGLE_WRITER`], so the fence
    /// check is a no-op.
    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError>;

    /// Persist a handle before its broadcast (persist-before-broadcast, so a crash
    /// is recoverable). Overwrites by [`id`](TxHandle::id).
    async fn put_handle(&self, handle: &TxHandle) -> Result<(), StateStoreError>;

    /// Non-terminal handles for `account`, for the executor to recover/track. The
    /// crash-recovery read: on boot these are the in-flight txs to rebroadcast.
    async fn pending_handles(&self, account: Address) -> Result<Vec<TxHandle>, StateStoreError>;

    /// A handle by id, **including terminal** ones (unlike [`pending_handles`]) — the
    /// status-query read: a `Confirmed`/`Failed`/`Replaced` handle is gone from
    /// `pending_handles` but still queryable here.
    async fn handle(&self, id: HandleId) -> Result<Option<TxHandle>, StateStoreError>;
}

/// Failures a durable store surfaces. The in-memory store never errors; redb/Postgres do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateStoreError {
    /// A backend I/O / query failure (redb, Postgres, …). Retryable.
    #[error("state store backend error: {source}")]
    Backend {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Encoding/decoding a persisted value failed (corrupt record / schema drift). Terminal.
    #[error("state (de)serialization failed: {source}")]
    Serialization {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The write carried a fence token lower than the highest committed for the scope — a
    /// superseded owner. Terminal: the caller must stop, not retry.
    #[error("write fenced: a newer owner holds this account")]
    Fenced,
    /// A blocking storage task (`spawn_blocking`) failed to join. Retryable.
    #[error("storage task failed: {0}")]
    Task(String),
}
