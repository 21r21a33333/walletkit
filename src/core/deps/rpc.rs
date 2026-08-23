use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, B256, Bytes, TxHash};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use async_trait::async_trait;

/// Object-safe read/submit facade over an alloy `Provider`: exactly the chain ops
/// the nonce/gas/submission adapters need. alloy's generic `Provider`/`Filler`
/// types stay confined inside the concrete adapter (the `Transport` struct, Task
/// 12) — only concrete data types cross this port.
#[async_trait]
pub trait Rpc: Send + Sync {
    async fn pending_nonce(&self, account: Address) -> Result<u64, RpcError>;
    /// Mined tx count for `account` at latest (the next mined nonce) — the executor's
    /// confirmation signal: a handle's nonce below this has been consumed on-chain.
    async fn tx_count(&self, account: Address) -> Result<u64, RpcError>;
    /// Latest block number, for confirmation depth.
    async fn block_number(&self) -> Result<u64, RpcError>;
    /// Latest finalized block number (the `finalized` tag), or `None` when the chain
    /// doesn't expose it (pre-merge, some L2s) so the caller falls back to a depth
    /// count. An outcome is treated as irreversible only at or below this height.
    async fn finalized_block(&self) -> Result<Option<u64>, RpcError>;
    /// Canonical hash of block `number`, or `None` if that block isn't on-chain yet.
    /// Anchors a receipt: a receipt whose block hash disagrees with this is a
    /// stale/reorged read and must not advance the tx lifecycle.
    async fn block_hash(&self, number: u64) -> Result<Option<B256>, RpcError>;
    async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError>;
    /// Base fee of the latest block (0 on pre-1559 chains).
    async fn base_fee(&self) -> Result<u128, RpcError>;
    /// `eth_estimateGas` — minimal sufficient gas (no end buffer; callers add drift).
    /// Executes the tx, so a deterministic `Err` also means it would revert.
    async fn estimate_gas(&self, request: &TransactionRequest) -> Result<u64, RpcError>;
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
