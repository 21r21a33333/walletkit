use alloy_primitives::Address;
use async_trait::async_trait;

/// Allocates gapless nonces for an account under single-writer ownership, and
/// resets the counter to reconcile with the chain after a detected gap.
#[async_trait]
pub trait NonceManager: Send + Sync {
    async fn allocate(&self, account: Address) -> Result<u64, NonceManagerError>;
    async fn reset(&self, account: Address, next: u64) -> Result<(), NonceManagerError>;
}

/// Variants grow with the manager adapter (CAS state store, Task 10).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NonceManagerError {}
