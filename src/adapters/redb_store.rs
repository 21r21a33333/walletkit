//! Durable embedded `StateStore` over redb (pure-Rust ACID KV). Sync redb runs inside
//! `spawn_blocking`. redb commits are `Durability::Immediate` by default (fsync before
//! `commit()` returns), which is exactly the persist-before-broadcast guarantee we need —
//! a handle reaches disk before it is broadcast and survives a restart. A durable store is
//! all crash recovery needs: the executor's per-tick `recover()`+`confirm()` reconciles
//! the persisted in-flight handles on the first tick after boot.

use crate::core::deps::{StateStore, StateStoreError, Versioned};
use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
use crate::obs::debug;
use alloy_primitives::Address;
use async_trait::async_trait;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;

// Ids are keyed as `&[u8]` (the 32 id bytes); `&[u8]`, `&str`, and tuples of them are
// first-class redb key types, so no custom `Key` impl is needed. NONCE stores the CAS
// tuple `(version, fence, NonceState)`; TX is the handle WAL; TX_PENDING is a secondary
// index of only the non-terminal handles, keyed by `(account, id)` for a prefix scan.
const NONCE: TableDefinition<&str, &[u8]> = TableDefinition::new("nonce");
const TX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx");
const TX_PENDING: TableDefinition<(&str, &[u8]), ()> = TableDefinition::new("tx_pending");

/// The full id-key range for one account's pending index: every 32-byte id from all-zeros
/// to all-ones, so a range scan over `(account, LO)..=(account, HI)` returns just that
/// account's non-terminal handles.
const ID_LO: [u8; 32] = [0u8; 32];
const ID_HI: [u8; 32] = [0xffu8; 32];

pub struct RedbStateStore {
    db: Arc<Database>,
}

impl RedbStateStore {
    /// Open (or create) a redb database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateStoreError> {
        let db = Database::create(path).map_err(backend)?;
        // Create the tables up front so the first read doesn't error on a missing table.
        let w = db.begin_write().map_err(backend)?;
        w.open_table(NONCE).map_err(backend)?;
        w.open_table(TX).map_err(backend)?;
        w.open_table(TX_PENDING).map_err(backend)?;
        w.commit().map_err(backend)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Run a synchronous redb transaction off the async runtime. redb is blocking, so every
    /// store method bridges through `spawn_blocking` over the shared `Arc<Database>`.
    async fn run<T, F>(&self, f: F) -> Result<T, StateStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Database) -> Result<T, StateStoreError> + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| StateStoreError::Task(e.to_string()))?
    }
}

fn backend<E: std::error::Error + Send + Sync + 'static>(e: E) -> StateStoreError {
    StateStoreError::Backend {
        source: Box::new(e),
    }
}

fn ser(e: serde_json::Error) -> StateStoreError {
    StateStoreError::Serialization {
        source: Box::new(e),
    }
}

/// The string key an account is stored under (nonce scope and pending index share it).
fn account_key(account: &Address) -> String {
    format!("{account:x}")
}

#[async_trait]
impl StateStore for RedbStateStore {
    async fn load_nonce_state(
        &self,
        scope: NonceScope,
    ) -> Result<Versioned<NonceState>, StateStoreError> {
        let key = account_key(&scope.account);
        self.run(move |db| {
            let r = db.begin_read().map_err(backend)?;
            let t = r.open_table(NONCE).map_err(backend)?;
            match t.get(key.as_str()).map_err(backend)? {
                Some(v) => {
                    let (version, _fence, value): (u64, FenceToken, NonceState) =
                        serde_json::from_slice(v.value()).map_err(ser)?;
                    Ok(Versioned { value, version })
                }
                None => Ok(Versioned::default()),
            }
        })
        .await
    }

    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError> {
        let key = account_key(&scope.account);
        let state = state.clone();
        let committed = self
            .run(move |db| {
                let w = db.begin_write().map_err(backend)?;
                let (cur_version, cur_fence) = {
                    let t = w.open_table(NONCE).map_err(backend)?;
                    match t.get(key.as_str()).map_err(backend)? {
                        Some(v) => {
                            let (ver, f, _): (u64, FenceToken, NonceState) =
                                serde_json::from_slice(v.value()).map_err(ser)?;
                            (ver, f)
                        }
                        None => (0, FenceToken::SINGLE_WRITER),
                    }
                };
                if fence < cur_fence {
                    return Err(StateStoreError::Fenced);
                }
                if cur_version != expected_version {
                    return Ok(false);
                }
                // Past the guard `fence >= cur_fence`, so it is already the new high-water mark.
                let bytes =
                    serde_json::to_vec(&(expected_version + 1, fence, &state)).map_err(ser)?;
                {
                    let mut t = w.open_table(NONCE).map_err(backend)?;
                    t.insert(key.as_str(), bytes.as_slice()).map_err(backend)?;
                }
                w.commit().map_err(backend)?;
                Ok(true)
            })
            .await?;
        if committed {
            debug!(account = %scope.account, "redb nonce committed");
        }
        Ok(committed)
    }

    async fn put_handle(&self, handle: &TxHandle) -> Result<(), StateStoreError> {
        let id = handle.id.as_bytes();
        let account = account_key(&handle.account);
        let terminal = handle.status.is_terminal();
        let bytes = serde_json::to_vec(handle).map_err(ser)?;
        self.run(move |db| {
            let w = db.begin_write().map_err(backend)?;
            {
                let mut t = w.open_table(TX).map_err(backend)?;
                t.insert(id.as_slice(), bytes.as_slice()).map_err(backend)?;
            }
            {
                let mut p = w.open_table(TX_PENDING).map_err(backend)?;
                let k: (&str, &[u8]) = (account.as_str(), id.as_slice());
                // A terminal handle leaves the pending index; a live one joins it.
                if terminal {
                    p.remove(k).map_err(backend)?;
                } else {
                    p.insert(k, ()).map_err(backend)?;
                }
            }
            w.commit().map_err(backend)?;
            Ok(())
        })
        .await?;
        debug!("redb handle persisted");
        Ok(())
    }

    async fn pending_handles(&self, account: Address) -> Result<Vec<TxHandle>, StateStoreError> {
        let key = account_key(&account);
        self.run(move |db| {
            let r = db.begin_read().map_err(backend)?;
            let ids: Vec<Vec<u8>> = {
                let p = r.open_table(TX_PENDING).map_err(backend)?;
                let lo: (&str, &[u8]) = (key.as_str(), &ID_LO);
                let hi: (&str, &[u8]) = (key.as_str(), &ID_HI);
                p.range(lo..=hi)
                    .map_err(backend)?
                    .map(|entry| entry.map(|(k, _)| k.value().1.to_vec()).map_err(backend))
                    .collect::<Result<_, _>>()?
            };
            let t = r.open_table(TX).map_err(backend)?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(v) = t.get(id.as_slice()).map_err(backend)? {
                    out.push(serde_json::from_slice(v.value()).map_err(ser)?);
                }
            }
            Ok(out)
        })
        .await
    }

    async fn handle(&self, id: HandleId) -> Result<Option<TxHandle>, StateStoreError> {
        let id = id.as_bytes();
        self.run(move |db| {
            let r = db.begin_read().map_err(backend)?;
            let t = r.open_table(TX).map_err(backend)?;
            match t.get(id.as_slice()).map_err(backend)? {
                Some(v) => Ok(Some(serde_json::from_slice(v.value()).map_err(ser)?)),
                None => Ok(None),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn redb_store_passes_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStateStore::open(dir.path().join("wk.redb")).unwrap();
        crate::testutils::state_store_conformance(Arc::new(store)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn redb_manager_passes_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStateStore::open(dir.path().join("wk.redb")).unwrap();
        crate::testutils::nonce_manager_conformance(Arc::new(store)).await;
    }

    #[tokio::test]
    async fn redb_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wk.redb");
        let account = Address::from([0x22; 20]);
        let scope = NonceScope::eoa(account);
        {
            let store = RedbStateStore::open(&path).unwrap();
            let s = NonceState {
                next: 9,
                ..Default::default()
            };
            store
                .cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER)
                .await
                .unwrap();
        } // drop closes the file
        let store = RedbStateStore::open(&path).unwrap();
        assert_eq!(store.load_nonce_state(scope).await.unwrap().value.next, 9);
    }

    // The end-to-end durability contract: an in-flight tx persisted by one wallet is
    // recovered and confirmed by a *fresh* wallet reopened over the same redb file — the
    // real crash-restart path (`redb_persists_across_reopen` only covers nonce state).
    #[tokio::test]
    async fn wallet_recovers_an_inflight_tx_after_restart_over_redb() {
        use crate::Wallet;
        use crate::core::wallet::TxStatus;
        use crate::testutils::{MockPolicy, MockRpc, MockSigner, intent, receipt};
        use alloy_primitives::B256;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wk.redb");
        let mined = B256::repeat_byte(1);
        // A chain view where the sent nonce is mined at block 8, anchored to `mined`, and
        // the head is deep past it — so one tick confirms at `confirmations(2)`.
        let rpc = || {
            Arc::new(MockRpc {
                tx_count: 1,
                block_number: 20,
                receipt: Some(receipt(true, 8, mined)),
                canonical: Some(mined),
                ..Default::default()
            })
        };
        let build = |store: Arc<RedbStateStore>| {
            Wallet::builder(
                rpc(),
                Arc::new(MockSigner::default()),
                Arc::new(MockPolicy::default()),
            )
            .confirmations(2)
            .store(store)
            .build()
        };

        // First instance: send, persisting a non-terminal handle to the redb file.
        let store1 = Arc::new(RedbStateStore::open(&path).unwrap());
        let id = {
            let w = build(store1);
            w.send(&intent()).await.expect("send").id
        }; // drop w (and its Arc<store>) => close the db

        // Restart: a fresh wallet over the SAME path recovers + confirms in one tick.
        let store2 = Arc::new(RedbStateStore::open(&path).unwrap());
        let w = build(store2);
        w.tick().await.expect("tick");
        assert!(
            matches!(
                w.status(id).await.expect("status"),
                Some(TxStatus::Confirmed { .. })
            ),
            "restarted wallet must reconcile the redb-persisted tx to Confirmed"
        );
    }
}
