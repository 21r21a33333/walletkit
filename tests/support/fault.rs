//! A fault-injecting [`Rpc`] decorator for confirmation-safety tests. It wraps the real
//! [`Transport`](walletkit::adapters::Transport) over anvil and delegates every call
//! unchanged **except** the two reads a false `Confirmed` hinges on — `block_hash` and
//! `block_number` — which it can be told to corrupt. This reproduces the one condition an
//! honest node (anvil) cannot: a receipt served from a block the chain has reorged away,
//! while the head keeps advancing.
//!
//! All faults are plain atomic flags flipped by the test between ticks — deterministic, no
//! RNG, no wall-clock — so a scenario reads as "lie, tick, assert; stop lying, tick, assert".

use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, B256, Bytes, TxHash};
use alloy_rpc_types_eth::{AccessListResult, TransactionReceipt, TransactionRequest};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use walletkit::core::deps::{AccountActivity, Rpc, RpcError, Simulated};

/// Fault switches shared with the test; flip them between `tick`s to inject/clear a fault.
#[derive(Default)]
pub struct Faults {
    corrupt_block_hash: AtomicBool,
    block_hash_none: AtomicBool,
    frozen_head: AtomicU64,
}

/// A bogus canonical hash — astronomically unlikely to equal a real receipt's block hash,
/// so returning it from `block_hash` forces the executor's anchoring check to fail.
const BOGUS_HASH: B256 = B256::repeat_byte(0xde);

impl Faults {
    /// Make `block_hash(n)` return a hash that cannot match any real receipt — the chain
    /// reorged block `n` to a different block (stale-fork read).
    pub fn corrupt_block_hash(&self, on: bool) {
        self.corrupt_block_hash.store(on, Ordering::Relaxed);
    }

    /// Make `block_hash(n)` return `None` — the node cannot resolve block `n`.
    pub fn block_hash_none(&self, on: bool) {
        self.block_hash_none.store(on, Ordering::Relaxed);
    }

    /// Pin `block_number()` to `height` (a stalled/lagging head); `0` clears the fault.
    pub fn freeze_head(&self, height: u64) {
        self.frozen_head.store(height, Ordering::Relaxed);
    }
}

/// The decorator: `inner` is the real transport; `faults` is the shared switch board.
pub struct FaultRpc {
    inner: Arc<dyn Rpc>,
    faults: Arc<Faults>,
}

impl FaultRpc {
    pub fn new(inner: Arc<dyn Rpc>, faults: Arc<Faults>) -> Self {
        Self { inner, faults }
    }
}

#[async_trait]
impl Rpc for FaultRpc {
    async fn block_number(&self) -> Result<u64, RpcError> {
        match self.faults.frozen_head.load(Ordering::Relaxed) {
            0 => self.inner.block_number().await,
            frozen => Ok(frozen),
        }
    }

    async fn block_hash(&self, number: u64) -> Result<Option<B256>, RpcError> {
        if self.faults.block_hash_none.load(Ordering::Relaxed) {
            return Ok(None);
        }
        if self.faults.corrupt_block_hash.load(Ordering::Relaxed) {
            return Ok(Some(BOGUS_HASH));
        }
        self.inner.block_hash(number).await
    }

    // Everything below is a faithful delegation — the honest node behind the fault.
    async fn pending_nonce(&self, account: Address) -> Result<u64, RpcError> {
        self.inner.pending_nonce(account).await
    }

    async fn tx_count(&self, account: Address) -> Result<u64, RpcError> {
        self.inner.tx_count(account).await
    }

    async fn finalized_block(&self) -> Result<Option<u64>, RpcError> {
        self.inner.finalized_block().await
    }

    async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
        self.inner.estimate_fees().await
    }

    async fn base_fee(&self) -> Result<u128, RpcError> {
        self.inner.base_fee().await
    }

    async fn estimate_gas(&self, request: &TransactionRequest) -> Result<u64, RpcError> {
        self.inner.estimate_gas(request).await
    }

    async fn call(&self, request: &TransactionRequest) -> Result<Simulated, RpcError> {
        self.inner.call(request).await
    }

    async fn create_access_list(
        &self,
        request: &TransactionRequest,
    ) -> Result<AccessListResult, RpcError> {
        self.inner.create_access_list(request).await
    }

    async fn send_raw(&self, rlp: Bytes) -> Result<TxHash, RpcError> {
        self.inner.send_raw(rlp).await
    }

    async fn receipt(&self, tx: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
        self.inner.receipt(tx).await
    }

    async fn account_activity(
        &self,
        accounts: &[Address],
    ) -> Result<Vec<AccountActivity>, RpcError> {
        self.inner.account_activity(accounts).await
    }
}
