//! CAS-based nonce management: [`LocalNonceManager`] is a load → compute →
//! compare-and-swap-retry loop whose atomicity lives entirely in the
//! [`StateStore`], so the *same* manager works single-process and distributed —
//! only the store implementation changes. [`InMemoryStateStore`] is the
//! single-process store. alloy's manager is not reused (not object-safe, known
//! recovery bugs); alloy is used only for the chain read via the [`Rpc`] port.
//!
//! # Assumptions & recovery (research-backed — see the plan's nonce section)
//! **Single writer.** The manager exclusively owns the account key — the universal
//! production posture (OZ Defender, thirdweb Engine). An out-of-band tx (the key
//! signed elsewhere) surfaces as `nonce too low` at submit and is recovered by
//! [`reset`](crate::core::deps::NonceManager::reset) to the chain's `latest` count;
//! the executor (Task 17) also reconciles against the chain each confirm cycle.
//! `pending` is only a forward hint (per-node/racy); `latest` is authoritative.
//!
//! **Crash recovery is NOT complete from `NonceState { next, free }` alone** — it
//! cannot tell an in-flight (broadcast) nonce from a dropped one. Durable recovery
//! needs (a) a durable `StateStore` (Phase 3) persisting `{next, free}` and (b) a
//! **persist-before-broadcast WAL of signed txs** (the persisted `TxHandle`, Task
//! 17): on boot `next = max(persisted, chain)`, drain the WAL rebroadcasting the
//! signed bytes (idempotent), and classify accepted → in-flight vs
//! deterministically-rejected → recycle. `InMemoryStateStore` is non-durable, so
//! Phase-1 recovery is single-run.

use crate::core::deps::{
    NonceManager, NonceManagerError, Rpc, StateStore, StateStoreError, Versioned,
};
use crate::core::wallet::{HandleId, NonceScope, NonceState, TxHandle};
use alloy_primitives::Address;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// In-memory store: a versioned map of `scope -> nonce state` plus persisted
/// handles. Non-durable, so recovery is single-run (see the module docs).
#[derive(Default)]
pub struct InMemoryStateStore {
    nonces: Mutex<HashMap<NonceScope, Versioned<NonceState>>>,
    handles: Mutex<HashMap<HandleId, TxHandle>>,
}

#[async_trait]
impl StateStore for InMemoryStateStore {
    async fn load_nonce_state(
        &self,
        scope: NonceScope,
    ) -> Result<Versioned<NonceState>, StateStoreError> {
        Ok(self
            .nonces
            .lock()
            .unwrap()
            .get(&scope)
            .cloned()
            .unwrap_or_default())
    }

    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
    ) -> Result<bool, StateStoreError> {
        let mut nonces = self.nonces.lock().unwrap();
        let current = nonces.get(&scope).map_or(0, |v| v.version);
        if current != expected_version {
            return Ok(false);
        }
        nonces.insert(
            scope,
            Versioned {
                value: state.clone(),
                version: expected_version + 1,
            },
        );
        Ok(true)
    }

    async fn put_handle(&self, handle: &TxHandle) -> Result<(), StateStoreError> {
        self.handles
            .lock()
            .unwrap()
            .insert(handle.id, handle.clone());
        Ok(())
    }
}

pub struct LocalNonceManager {
    store: Arc<dyn StateStore>,
    rpc: Arc<dyn Rpc>,
}

impl LocalNonceManager {
    pub fn new(store: Arc<dyn StateStore>, rpc: Arc<dyn Rpc>) -> Self {
        Self { store, rpc }
    }
}

#[async_trait]
impl NonceManager for LocalNonceManager {
    async fn allocate(&self, account: Address) -> Result<u64, NonceManagerError> {
        let scope = NonceScope::eoa(account);
        loop {
            let Versioned { mut value, version } = self.store.load_nonce_state(scope).await?;
            if version == 0 {
                value.next = self.rpc.pending_nonce(account).await?; // reconcile on first use
            }
            let nonce = match value.free.iter().next().copied() {
                Some(n) => {
                    value.free.remove(&n); // recycle the lowest freed nonce first
                    n
                }
                None => {
                    let n = value.next;
                    value.next += 1;
                    n
                }
            };
            if self.store.cas_nonce_state(scope, version, &value).await? {
                return Ok(nonce);
            }
            // a concurrent writer/replica advanced the version -> retry
        }
    }

    async fn release(&self, account: Address, nonce: u64) -> Result<(), NonceManagerError> {
        let scope = NonceScope::eoa(account);
        loop {
            let Versioned { mut value, version } = self.store.load_nonce_state(scope).await?;
            if nonce >= value.next {
                return Ok(()); // not a live reservation
            }
            if nonce + 1 == value.next {
                value.next -= 1; // releasing the top: shrink the high-water mark...
                while value.next > 0 && value.free.remove(&(value.next - 1)) {
                    value.next -= 1; // ...and absorb any now-contiguous freed nonces
                }
            } else {
                value.free.insert(nonce); // a middle gap: recycle it later
            }
            if self.store.cas_nonce_state(scope, version, &value).await? {
                return Ok(());
            }
        }
    }

    async fn reset(&self, account: Address, chain_next: u64) -> Result<(), NonceManagerError> {
        let scope = NonceScope::eoa(account);
        loop {
            let Versioned { mut value, version } = self.store.load_nonce_state(scope).await?;
            value.next = value.next.max(chain_next); // forward only
            value.free.retain(|&n| n >= chain_next); // drop freed nonces consumed on-chain
            if self.store.cas_nonce_state(scope, version, &value).await? {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::RpcError;
    use alloy_eips::eip1559::Eip1559Estimation;
    use alloy_primitives::{Bytes, TxHash};
    use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};

    /// Fixed pending-nonce source; the other RPC ops are never hit by these tests.
    struct FakeRpc {
        pending: u64,
    }

    #[async_trait]
    impl Rpc for FakeRpc {
        async fn pending_nonce(&self, _account: Address) -> Result<u64, RpcError> {
            Ok(self.pending)
        }
        async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
            unreachable!("not used by nonce tests")
        }
        async fn base_fee(&self) -> Result<u128, RpcError> {
            unreachable!("not used by nonce tests")
        }
        async fn estimate_gas(&self, _: &TransactionRequest) -> Result<u64, RpcError> {
            unreachable!("not used by nonce tests")
        }
        async fn send_raw(&self, _rlp: Bytes) -> Result<TxHash, RpcError> {
            unreachable!("not used by nonce tests")
        }
        async fn receipt(&self, _tx: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
            unreachable!("not used by nonce tests")
        }
    }

    fn manager(pending: u64) -> LocalNonceManager {
        LocalNonceManager::new(
            Arc::new(InMemoryStateStore::default()),
            Arc::new(FakeRpc { pending }),
        )
    }

    #[tokio::test]
    async fn allocates_gaplessly_reconciling_from_chain_on_first_use() {
        let m = manager(5);
        let a = Address::ZERO;
        assert_eq!(m.allocate(a).await.unwrap(), 5);
        assert_eq!(m.allocate(a).await.unwrap(), 6);
        assert_eq!(m.allocate(a).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn release_top_shrinks_high_water_and_absorbs_contiguous_freed() {
        let m = manager(5);
        let a = Address::ZERO;
        for _ in 0..3 {
            m.allocate(a).await.unwrap(); // 5,6,7 -> next=8
        }
        m.release(a, 6).await.unwrap(); // middle gap -> free={6}
        m.release(a, 7).await.unwrap(); // top -> next 8->7, absorbs 6 -> next=6
        assert_eq!(m.allocate(a).await.unwrap(), 6);
    }

    #[tokio::test]
    async fn release_middle_recycles_lowest_first() {
        let m = manager(5);
        let a = Address::ZERO;
        for _ in 0..3 {
            m.allocate(a).await.unwrap(); // next=8
        }
        m.release(a, 6).await.unwrap(); // free={6}
        assert_eq!(m.allocate(a).await.unwrap(), 6); // recycle freed first
        assert_eq!(m.allocate(a).await.unwrap(), 8); // then fresh
    }

    #[tokio::test]
    async fn reset_moves_forward_only_on_nonce_too_low() {
        let m = manager(5);
        let a = Address::ZERO;
        for _ in 0..3 {
            m.allocate(a).await.unwrap(); // next=8
        }
        m.reset(a, 10).await.unwrap(); // jump forward
        assert_eq!(m.allocate(a).await.unwrap(), 10);
        m.reset(a, 3).await.unwrap(); // stale reset does not move backward
        assert_eq!(m.allocate(a).await.unwrap(), 11);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_allocations_never_duplicate() {
        let m = Arc::new(manager(5));
        let a = Address::ZERO;
        let handles: Vec<_> = (0..50)
            .map(|_| {
                let m = m.clone();
                tokio::spawn(async move { m.allocate(a).await.unwrap() })
            })
            .collect();

        let mut nonces = Vec::new();
        for h in handles {
            nonces.push(h.await.unwrap());
        }
        nonces.sort_unstable();
        assert_eq!(nonces, (5..55).collect::<Vec<_>>()); // 50 unique & contiguous
    }
}
