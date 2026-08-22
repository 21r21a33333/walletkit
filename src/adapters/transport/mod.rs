//! `Transport` — the one concrete [`Rpc`] adapter. It wraps an alloy provider
//! (type-erased into a [`DynProvider`], so the struct is concrete and non-generic)
//! and reuses alloy's transport layers for reliability. Construction lives in
//! [`build`]: [`Transport::builder`] for full control, [`Transport::single`] for a
//! single HTTP endpoint, or [`Transport::from_config`] from a declarative
//! [`TransportConfig`] (per-chain, config-file friendly).
//!
//! # In-process knobs vs eRPC
//! The builder exposes the resilience features achievable **in-process** with
//! alloy: multi-endpoint **failover/hedge**, **retry/backoff**, per-request
//! **timeout**, **auth headers**, and client-side **rate-limit/throttle**. The
//! *stateful/coordinated* features — reorg-aware caching, cross-upstream quorum,
//! provider auto-discovery, per-method routing — are **not** reimplemented here;
//! run **[eRPC](https://github.com/erpc/erpc)** and point one endpoint at it
//! (`Transport::single(erpc_url)`). Per chain, hold a `chain_id -> Transport` map
//! (assembled by the facade) built from one [`TransportConfig`] each.

mod build;
mod chains;

pub use build::{TransportBuilder, TransportConfig};
pub use chains::{Vendor, public_rpcs, refresh_public_endpoints, vendor_url};

use crate::core::deps::{Rpc, RpcError};
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, Bytes, TxHash};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_transport::{RpcError as AlloyRpcError, TransportError};
use async_trait::async_trait;

pub struct Transport {
    provider: DynProvider,
}

#[async_trait]
impl Rpc for Transport {
    async fn pending_nonce(&self, account: Address) -> Result<u64, RpcError> {
        self.provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(rpc_err)
    }

    async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
        self.provider.estimate_eip1559_fees().await.map_err(rpc_err)
    }

    async fn send_raw(&self, rlp: Bytes) -> Result<TxHash, RpcError> {
        let pending = self
            .provider
            .send_raw_transaction(rlp.as_ref())
            .await
            .map_err(rpc_err)?;
        Ok(*pending.tx_hash())
    }

    async fn receipt(&self, tx: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
        self.provider
            .get_transaction_receipt(tx)
            .await
            .map_err(rpc_err)
    }
}

/// Map an alloy transport error to our port error. A transport-level failure
/// (network/timeout/5xx/429, via alloy's `is_retry_err`) is transient/retryable; a
/// JSON-RPC method error (e.g. "nonce too low") is terminal.
fn rpc_err(e: TransportError) -> RpcError {
    let transient = matches!(&e, AlloyRpcError::Transport(kind) if kind.is_retry_err());
    RpcError::Call {
        transient,
        message: e.to_string(),
    }
}
