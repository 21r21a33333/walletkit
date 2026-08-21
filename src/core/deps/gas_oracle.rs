use alloy_eips::eip1559::Eip1559Estimation;
use async_trait::async_trait;

/// Estimates EIP-1559 fees (base-fee-aware max fee + priority tip) for a chain.
#[async_trait]
pub trait GasOracle: Send + Sync {
    async fn estimate(&self, chain_id: u64) -> Result<Eip1559Estimation, GasOracleError>;
}

/// Variants grow with the oracle adapter (Task 13).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GasOracleError {}
