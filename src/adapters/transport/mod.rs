//! `Transport` — the one concrete [`Rpc`] adapter. It wraps an alloy provider
//! (type-erased into a [`DynProvider`], so the struct is concrete and non-generic)
//! and reuses alloy's transport layers for reliability. Construction lives in
//! `build`: [`Transport::builder`] for full control, [`Transport::url`] for a
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
//! (`Transport::url(erpc_url)`). Per chain, hold a `chain_id -> Transport` map
//! (assembled by the facade) built from one [`TransportConfig`] each.

mod build;
mod chains;

pub use build::{TransportBuildError, TransportBuilder, TransportConfig};
pub use chains::{Vendor, public_rpcs, refresh_public_endpoints, vendor_url};

use crate::core::deps::{AccountActivity, Rpc, RpcError, Simulated};
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Address, B256, Bytes, TxHash, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_client::BatchRequest;
use alloy_rpc_types_eth::{AccessListResult, TransactionReceipt, TransactionRequest};
use alloy_transport::{RpcError as AlloyRpcError, TransportError};
use async_trait::async_trait;

pub struct Transport {
    provider: DynProvider,
}

impl Transport {
    /// A clone of the resilient provider this transport wraps, for read-only adapters
    /// ([`RpcReadClient`](crate::adapters::RpcReadClient), ENS) that need typed
    /// `sol!`/multicall access yet must inherit the same failover/retry/hedge as the
    /// write path. `DynProvider` is `Arc`-backed, so the clone is cheap.
    pub fn provider(&self) -> DynProvider {
        self.provider.clone()
    }
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

    async fn tx_count(&self, account: Address) -> Result<u64, RpcError> {
        self.provider
            .get_transaction_count(account)
            .await
            .map_err(rpc_err)
    }

    async fn block_number(&self) -> Result<u64, RpcError> {
        self.provider.get_block_number().await.map_err(rpc_err)
    }

    async fn finalized_block(&self) -> Result<Option<u64>, RpcError> {
        // A node that doesn't expose the `finalized` tag returns null (`Ok(None)`);
        // the caller then falls back to a depth count. Errors stay transient/terminal.
        Ok(self
            .provider
            .get_block(BlockId::finalized())
            .await
            .map_err(rpc_err)?
            .map(|block| block.header.number))
    }

    async fn block_hash(&self, number: u64) -> Result<Option<B256>, RpcError> {
        Ok(self
            .provider
            .get_block(BlockId::number(number))
            .await
            .map_err(rpc_err)?
            .map(|block| block.header.hash))
    }

    async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
        self.provider.estimate_eip1559_fees().await.map_err(rpc_err)
    }

    async fn base_fee(&self) -> Result<u128, RpcError> {
        let block = self
            .provider
            .get_block(BlockId::latest())
            .await
            .map_err(rpc_err)?
            .ok_or_else(|| RpcError::Call {
                transient: true,
                message: "latest block unavailable".into(),
            })?;
        Ok(block.header.base_fee_per_gas.unwrap_or_default() as u128)
    }

    async fn estimate_gas(&self, request: &TransactionRequest) -> Result<u64, RpcError> {
        self.provider
            .estimate_gas(request.clone())
            .await
            .map_err(rpc_err)
    }

    async fn call(&self, request: &TransactionRequest) -> Result<Simulated, RpcError> {
        match self.provider.call(request.clone()).await {
            Ok(data) => Ok(Simulated::Returned(data)),
            // A contract revert carries its data on the JSON-RPC error; surface that as a
            // successful simulation. Anything without revert data is a real transport error.
            Err(e) => match e.as_error_resp().and_then(|resp| resp.as_revert_data()) {
                Some(revert) => Ok(Simulated::Reverted(revert)),
                None => Err(rpc_err(e)),
            },
        }
    }

    async fn create_access_list(
        &self,
        request: &TransactionRequest,
    ) -> Result<AccessListResult, RpcError> {
        self.provider
            .create_access_list(request)
            .await
            .map_err(rpc_err)
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

    async fn account_activity(
        &self,
        accounts: &[Address],
    ) -> Result<Vec<AccountActivity>, RpcError> {
        if accounts.is_empty() {
            return Ok(Vec::new());
        }
        // One JSON-RPC batch carries eth_getTransactionCount + eth_getBalance for every
        // account, so a discovery window is a single HTTP round-trip. `send()` yields a
        // 'static future, so the client borrow is released before the await.
        let mut batch = BatchRequest::new(self.provider.client());
        let mut waiters = Vec::with_capacity(accounts.len());
        for &account in accounts {
            let params = (account, BlockNumberOrTag::Latest);
            let nonce = batch
                .add_call::<_, U256>("eth_getTransactionCount", &params)
                .map_err(rpc_err)?;
            let balance = batch
                .add_call::<_, U256>("eth_getBalance", &params)
                .map_err(rpc_err)?;
            waiters.push((nonce, balance));
        }
        batch.send().await.map_err(rpc_err)?;
        let mut out = Vec::with_capacity(accounts.len());
        for (nonce, balance) in waiters {
            out.push(AccountActivity {
                nonce: nonce.await.map_err(rpc_err)?.saturating_to::<u64>(),
                balance: balance.await.map_err(rpc_err)?,
            });
        }
        Ok(out)
    }
}

/// Map an alloy transport error to our port error. A transport-level failure
/// (network/timeout/5xx/429, via alloy's `is_retry_err`) is transient/retryable; a
/// JSON-RPC method error (e.g. "nonce too low") is terminal. Shared with the read adapter.
pub(crate) fn rpc_err(e: TransportError) -> RpcError {
    let transient = matches!(&e, AlloyRpcError::Transport(kind) if kind.is_retry_err());
    RpcError::Call {
        transient,
        message: e.to_string(),
    }
}
