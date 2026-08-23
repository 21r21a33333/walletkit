use crate::core::deps::{RpcError, StateStoreError};
use alloy_primitives::Address;
use async_trait::async_trait;

/// Allocates gapless nonces for an account under single-writer ownership, recycles
/// released reservations, and reconciles with the chain after a detected gap.
#[async_trait]
pub trait NonceManager: Send + Sync {
    async fn allocate(&self, account: Address) -> Result<u64, NonceManagerError>;
    /// Recycle a reserved nonce whose transaction will **never mine** (never
    /// broadcast, or dropped from the mempool). Never release a nonce whose tx was
    /// mined — even a reverted tx consumes its nonce, so recycling it would cause a
    /// `nonce too low` collision.
    async fn release(&self, account: Address, nonce: u64) -> Result<(), NonceManagerError>;
    /// Reconcile the counter forward to the chain's next nonce (`nonce too low` recovery).
    async fn reset(&self, account: Address, next: u64) -> Result<(), NonceManagerError>;
}

/// Wraps the operational failures the manager surfaces from its dependencies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NonceManagerError {
    #[error(transparent)]
    Store(#[from] StateStoreError),
    #[error(transparent)]
    Rpc(#[from] RpcError),
}
