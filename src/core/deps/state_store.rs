use alloy_primitives::Address;
use async_trait::async_trait;

/// Durable state behind the executor. Phase 1 persists only the per-account nonce
/// counter; the idempotency map, tx handles, and pending intents are added by the
/// tasks that first consume them.
#[async_trait]
pub trait StateStore: Send + Sync {
    async fn load_nonce(&self, account: Address) -> Result<Option<u64>, StateStoreError>;
    async fn store_nonce(&self, account: Address, next: u64) -> Result<(), StateStoreError>;
}

/// Variants grow with the store adapters (in-memory Task 10, durable later).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateStoreError {}
