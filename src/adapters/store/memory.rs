//! In-memory `StateStore`: a versioned map of `scope -> nonce state` plus persisted
//! handles. Non-durable, so recovery is single-run — the durable backends are
//! the feature-gated `redb` and `postgres` stores.

use crate::core::deps::{StateStore, StateStoreError, Versioned};
use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
use alloy_primitives::Address;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;

/// The non-durable [`StateStore`]: keeps nonce state and
/// handles in memory. Recovery is single-run only; use redb/Postgres to survive a restart.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // Held to the same backend-agnostic contract as redb/Postgres.
    #[tokio::test]
    async fn in_memory_store_passes_conformance() {
        crate::testutils::state_store_conformance(Arc::new(InMemoryStateStore::default())).await;
    }
}
