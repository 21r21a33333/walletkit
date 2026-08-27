//! CAS-based nonce management: [`LocalNonceManager`] is a load → compute →
//! compare-and-swap-retry loop whose atomicity lives entirely in the
//! [`StateStore`], so the *same* manager works single-process and distributed —
//! only the store implementation changes. alloy's manager is not reused (not
//! object-safe, known recovery bugs); alloy is used only for the chain read via the
//! [`Rpc`] port.
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
//! (idempotent), and classify accepted → in-flight vs rejected → recycle. The in-memory
//! store is non-durable, so its recovery is single-run.

use crate::core::deps::{NonceManager, NonceManagerError, Rpc, StateStore, Versioned};
use crate::core::wallet::{FenceToken, NonceScope, NonceState};
use crate::obs::debug;
use alloy_primitives::Address;
use async_trait::async_trait;
use std::sync::Arc;

/// The [`NonceManager`] backed by a [`StateStore`] CAS,
/// under the single-writer-per-account fencing default.
pub struct LocalNonceManager {
    store: Arc<dyn StateStore>,
    rpc: Arc<dyn Rpc>,
    /// The fence carried on every CAS. Single-writer-per-account is the documented
    /// default (SPEC §7); a distributed lease issuer will supply a real token later.
    fence: FenceToken,
}

impl LocalNonceManager {
    /// Build over a durable store and an RPC (for chain reconciliation).
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
    use crate::adapters::store::InMemoryStateStore;

    // The allocate/release/reset/concurrent contract lives in the shared
    // `nonce_manager_conformance` suite (run here + against redb + Postgres).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn in_memory_manager_passes_conformance() {
        crate::testutils::nonce_manager_conformance(Arc::new(InMemoryStateStore::default())).await;
    }
}
