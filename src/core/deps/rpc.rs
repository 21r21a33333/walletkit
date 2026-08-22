use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, Bytes, TxHash};
use alloy_rpc_types_eth::TransactionReceipt;
use async_trait::async_trait;

/// Object-safe read/submit facade over an alloy `Provider`: exactly the chain ops
/// the nonce/gas/submission adapters need. alloy's generic `Provider`/`Filler`
/// types stay confined inside the concrete adapter (the `Transport` struct, Task
/// 12) — only concrete data types cross this port.
#[async_trait]
pub trait Rpc: Send + Sync {
    async fn pending_nonce(&self, account: Address) -> Result<u64, RpcError>;
    async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError>;
    /// Base fee of the latest block (0 on pre-1559 chains).
    async fn base_fee(&self) -> Result<u128, RpcError>;
    async fn send_raw(&self, rlp: Bytes) -> Result<TxHash, RpcError>;
    async fn receipt(&self, tx: TxHash) -> Result<Option<TransactionReceipt>, RpcError>;
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// An RPC call failed. `transient` (network/timeout/5xx/rate-limit) → the caller
    /// may retry; otherwise it is a terminal JSON-RPC or method error.
    #[error("rpc call failed: {message}")]
    Call { message: String, transient: bool },
}
