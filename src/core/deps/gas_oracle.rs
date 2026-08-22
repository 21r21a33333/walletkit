use crate::core::deps::RpcError;
use alloy_eips::eip1559::Eip1559Estimation;
use async_trait::async_trait;

/// EIP-1559 fee pricing for a chain-bound RPC: an initial estimate and the
/// replacement (RBF) bump used to unstick a pending tx (same nonce — Task 17).
#[async_trait]
pub trait GasOracle: Send + Sync {
    /// Base-fee-aware max fee + priority tip (alloy's default estimator).
    async fn estimate(&self) -> Result<Eip1559Estimation, GasOracleError>;

    /// Reprice `prev` for a same-nonce replacement: geth's price bump on both fields
    /// plus base-fee coverage. Errors at the ceiling rather than looping.
    async fn bump(&self, prev: Eip1559Estimation) -> Result<Eip1559Estimation, GasOracleError>;
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GasOracleError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Bumped max fee would exceed the per-tx ceiling — stop bumping, don't overpay.
    #[error("bumped max fee {needed} wei exceeds ceiling {ceiling} wei")]
    CeilingExceeded { ceiling: u128, needed: u128 },
}
