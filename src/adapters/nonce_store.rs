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
use crate::obs::debug;
use alloy_primitives::Address;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

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
        Ok(self.nonces.lock().get(&scope).cloned().unwrap_or_default())
    }

    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
    ) -> Result<bool, StateStoreError> {
        let mut nonces = self.nonces.lock();
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
        self.handles.lock().insert(handle.id, handle.clone());
        Ok(())
    }

    async fn pending_handles(&self, account: Address) -> Result<Vec<TxHandle>, StateStoreError> {
        Ok(self
            .handles
            .lock()
            .values()
            .filter(|h| h.account == account && !h.status.is_terminal())
            .cloned()
            .collect())
    }

    async fn handle(&self, id: HandleId) -> Result<Option<TxHandle>, StateStoreError> {
        Ok(self.handles.lock().get(&id).cloned())
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
                debug!(nonce, "nonce assigned");
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
                debug!(chain_next, "nonce reconciled to chain");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::wallet::TxStatus;
    use crate::testutils::{MockRpc, handle};

    fn manager(pending: u64) -> LocalNonceManager {
        LocalNonceManager::new(
            Arc::new(InMemoryStateStore::default()),
            Arc::new(MockRpc {
                pending_nonce: pending,
                ..Default::default()
            }),
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

    #[tokio::test]
    async fn reset_retains_high_freed_nonce_and_drops_consumed_freed() {
        // reset() drops freed nonces the chain already consumed but keeps those at or
        // above the chain's next — the `>=` boundary, not `>`. The at-boundary element
        // (75) is what distinguishes the two; the plain fixtures elsewhere can't.
        use std::collections::BTreeSet;
        let store = Arc::new(InMemoryStateStore::default());
        let m = LocalNonceManager::new(
            store.clone(),
            Arc::new(MockRpc {
                pending_nonce: 0,
                ..Default::default()
            }),
        );
        let a = Address::ZERO;
        let scope = NonceScope::eoa(a);
        let seeded = NonceState {
            next: 100,
            free: BTreeSet::from([74, 75, 150]),
        };
        assert!(store.cas_nonce_state(scope, 0, &seeded).await.unwrap());

        m.reset(a, 75).await.unwrap();

        let after = store.load_nonce_state(scope).await.unwrap().value;
        assert_eq!(after.next, 100); // max(100, 75) — forward only
        assert_eq!(after.free, BTreeSet::from([75, 150])); // 74 dropped, 75 kept (>=), 150 kept

        // Recycle lowest-first across the reset boundary, then fresh from `next`.
        assert_eq!(m.allocate(a).await.unwrap(), 75);
        assert_eq!(m.allocate(a).await.unwrap(), 150);
        assert_eq!(m.allocate(a).await.unwrap(), 100);
        assert_eq!(m.allocate(a).await.unwrap(), 101);
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

    #[tokio::test]
    async fn pending_handles_excludes_terminal() {
        let store = InMemoryStateStore::default();
        let acct = Address::ZERO;
        store.put_handle(&handle(1, TxStatus::Sent)).await.unwrap();
        store
            .put_handle(&handle(2, TxStatus::Confirmed { block: 1 }))
            .await
            .unwrap();
        store
            .put_handle(&handle(3, TxStatus::Replaced))
            .await
            .unwrap();

        let pending = store.pending_handles(acct).await.unwrap();
        assert_eq!(pending.len(), 1); // only the Sent one; terminal excluded
        assert_eq!(pending[0].nonce, 1);
    }

    #[tokio::test]
    async fn handle_returns_by_id_including_terminal() {
        let store = InMemoryStateStore::default();
        let sent = handle(1, TxStatus::Sent);
        let done = handle(2, TxStatus::Confirmed { block: 9 });
        store.put_handle(&sent).await.unwrap();
        store.put_handle(&done).await.unwrap();

        assert_eq!(store.handle(sent.id).await.unwrap().unwrap().nonce, 1);
        // terminal handles are gone from pending_handles but still readable by id:
        assert_eq!(
            store.handle(done.id).await.unwrap().unwrap().status,
            TxStatus::Confirmed { block: 9 }
        );
        let missing = handle(99, TxStatus::Sent).id;
        assert!(store.handle(missing).await.unwrap().is_none());
    }
}
