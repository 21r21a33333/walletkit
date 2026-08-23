//! CAS-based nonce management: [`LocalNonceManager`] is a load → compute →
//! compare-and-swap-retry loop whose atomicity lives entirely in the
//! [`StateStore`], so the *same* manager works single-process and distributed —
//! only the store implementation changes. [`InMemoryStateStore`] is the
//! single-process store. alloy's manager is not reused (not object-safe, known
//! recovery bugs); alloy is used only for the chain read via the [`Rpc`] port.
//!
//! # Assumptions & recovery
//! **Single writer.** The manager exclusively owns the account key — the universal
//! production posture (OZ Defender, thirdweb Engine). An out-of-band tx (the key
//! signed elsewhere) surfaces as `nonce too low` at submit and is recovered by
//! [`reset`](crate::core::deps::NonceManager::reset) to the chain's `latest` count;
//! the executor also reconciles against the chain each confirm cycle. `pending` is only
//! a forward hint (per-node/racy); `latest` is authoritative.
//!
//! **Crash recovery is NOT complete from `NonceState { next, free }` alone** — it
//! cannot tell an in-flight (broadcast) nonce from a dropped one. Durable recovery needs
//! a durable `StateStore` plus the persist-before-broadcast `TxHandle` WAL: on boot
//! `next = max(persisted, chain)`, drain the WAL rebroadcasting the signed bytes
//! (idempotent), and classify accepted → in-flight vs rejected → recycle.
//! `InMemoryStateStore` is non-durable, so its recovery is single-run.

use crate::core::deps::{
    NonceManager, NonceManagerError, Rpc, StateStore, StateStoreError, Versioned,
};
use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
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
    nonces: Mutex<HashMap<NonceScope, (Versioned<NonceState>, FenceToken)>>,
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
            .get(&scope)
            .map(|(v, _)| v.clone())
            .unwrap_or_default())
    }

    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError> {
        let mut nonces = self.nonces.lock();
        let (cur_version, cur_fence) = nonces
            .get(&scope)
            .map_or((0, FenceToken::SINGLE_WRITER), |(v, f)| (v.version, *f));
        if fence < cur_fence {
            return Err(StateStoreError::Fenced);
        }
        if cur_version != expected_version {
            return Ok(false);
        }
        // Past the guard, `fence >= cur_fence`, so it is already the new high-water mark.
        let entry = (
            Versioned {
                value: state.clone(),
                version: expected_version + 1,
            },
            fence,
        );
        nonces.insert(scope, entry);
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
    /// The fence carried on every CAS. Single-writer-per-account is the documented
    /// default (SPEC §7); a distributed lease issuer will supply a real token later.
    fence: FenceToken,
}

impl LocalNonceManager {
    pub fn new(store: Arc<dyn StateStore>, rpc: Arc<dyn Rpc>) -> Self {
        Self {
            store,
            rpc,
            fence: FenceToken::SINGLE_WRITER,
        }
    }

    /// CAS the scope's state carrying this manager's fence, so the single-writer token is
    /// threaded in one place rather than at every allocate/release/reset call site.
    async fn cas(
        &self,
        scope: NonceScope,
        version: u64,
        value: &NonceState,
    ) -> Result<bool, NonceManagerError> {
        Ok(self
            .store
            .cas_nonce_state(scope, version, value, self.fence)
            .await?)
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
            if self.cas(scope, version, &value).await? {
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
            if self.cas(scope, version, &value).await? {
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
            if self.cas(scope, version, &value).await? {
                debug!(chain_next, "nonce reconciled to chain");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The manager's allocate/release/reset/concurrent behavior lives in the shared
    // `nonce_manager_conformance` suite (run here + against redb + Postgres), and the store
    // contract — including fence rejection — in `state_store_conformance`. Keeping both here
    // means the in-memory store is held to the exact same bar as the durable backends.
    #[tokio::test]
    async fn in_memory_store_passes_conformance() {
        crate::testutils::state_store_conformance(Arc::new(InMemoryStateStore::default())).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_manager_passes_conformance() {
        crate::testutils::nonce_manager_conformance(Arc::new(InMemoryStateStore::default())).await;
    }
}
