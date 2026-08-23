# Sub-project B — Durable State & Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make walletkit's `StateStore` durable (survives process restarts) with two backends behind the existing port — embedded **redb** and networked **Postgres** — and reserve the single-writer **`FenceToken`** seam.

**Architecture:** Add an opaque monotonic `FenceToken` to the `StateStore` nonce CAS (single-writer sentinel now; reject-if-lower enforced at the store, per Kleppmann). Give `StateStoreError` real variants. Add `serde` to the handle types. Implement `RedbStateStore` (sync redb bridged via `spawn_blocking`) and `PostgresStateStore` (`sqlx`, pure-Rust driver), both behind additive feature flags, both validated by one backend-agnostic conformance suite. Recovery needs no new code — a durable store makes the existing per-tick `recover()`+`confirm()` reconcile on the first tick after restart.

**Tech Stack:** Rust edition 2024; `redb` 4.x (optional, default-on); `sqlx` 0.8 postgres (optional, default-off); `serde_json` codec (already a dep); `tempfile` (dev).

## Global Constraints

- **Review-gated (CLAUDE.md):** each task — write the whole task's code, run `cargo fmt --all --check` + `cargo clippy --all-targets` (+ `cargo clippy --no-default-features` and/or `--features postgres` where relevant) + `cargo test`, report real output, leave **uncommitted**, commit **only on approval**. No `Co-Authored-By` trailer.
- **No `unwrap`/`expect`/`panic` in prod** (tests/`const`/documented-infallible only); prefer `parking_lot`; `match`/combinators over `if/else` ladders; DRY; YAGNI.
- **Observability + errors (A-phase standards, now binding):** public failures return `WalletKitError` classified via `kind()`; instrument via `crate::obs` (import once, call bare) + `#[cfg_attr(feature="tracing", tracing::instrument(...))]`; `skip_all` on key paths; green **with and without** `--no-default-features`.
- **Additive features:** `redb` (default-on), `postgres` (default-off); `--no-default-features` builds the trait + `InMemoryStateStore` only.
- redb commits use `Durability::Immediate` (persist-before-broadcast). Codec = `serde_json`.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/core/wallet/primitives/nonce.rs` | add `FenceToken` |
| `src/core/deps/state_store.rs` | `cas_nonce_state(+fence)`; real `StateStoreError` variants; doc the two guarantees |
| `src/core/wallet/primitives/handle.rs` | `serde` on `TxHandle`/`TxStatus`/`HandleId` |
| `src/core/wallet/primitives/policy.rs` | `serde` on `GasEnvelope` |
| `src/adapters/nonce_store.rs` | `InMemoryStateStore` fence handling; `LocalNonceManager` fence field |
| `src/error.rs` | `store_kind` matches real `StateStoreError` variants |
| `src/testutils.rs` | `MockStore` new CAS sig; `state_store_conformance` suite |
| `src/adapters/redb_store.rs` (new) | redb adapter (feature `redb`) |
| `src/adapters/postgres_store.rs` (new) | Postgres adapter (feature `postgres`) |
| `src/adapters/mod.rs` | feature-gated re-exports |
| `Cargo.toml` | redb/sqlx optional deps + features + `tempfile` dev-dep |
| `.github/workflows/ci.yml` | Postgres service job for `--features postgres` |

---

## Task 1: Port + `FenceToken` + serde

**Files:** Modify `src/core/wallet/primitives/nonce.rs`, `src/core/deps/state_store.rs`, `src/core/wallet/primitives/handle.rs`, `src/core/wallet/primitives/policy.rs`, `src/adapters/nonce_store.rs`, `src/testutils.rs`, `src/error.rs`, `src/core/wallet/mod.rs` (re-export `FenceToken`).

**Interfaces produced:** `FenceToken` (+ `SINGLE_WRITER`); `cas_nonce_state(scope, expected_version, state, fence) -> Result<bool, StateStoreError>`; `StateStoreError::{Backend,Serialization,Fenced,Task}`.

- [ ] **Step 1: Add `FenceToken`** to `nonce.rs` (after `NonceState`):

```rust
/// Opaque, monotonic ownership token for the single-writer nonce seam. The store
/// records the highest token committed per scope and rejects any lower one (fencing
/// enforced at the resource, per Kleppmann). Phase 1 uses `SINGLE_WRITER` only; a
/// distributed lease issuer mints real tokens later with no trait change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FenceToken(u64);

impl FenceToken {
    /// The sole token in single-writer mode — every write carries it, so the
    /// reject-if-lower check is always satisfied (a no-op) until a lease issuer exists.
    pub const SINGLE_WRITER: FenceToken = FenceToken(0);
}
```
Re-export from `core/wallet/mod.rs` (`primitives::{… FenceToken …}`) and it will already surface under `crate::core::wallet`.

- [ ] **Step 2: `StateStoreError` variants** — replace the empty enum in `state_store.rs`:

```rust
/// Failures a durable store surfaces. The in-memory store never errors; redb/Postgres do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateStoreError {
    /// A backend I/O / query failure (redb, Postgres, …). Retryable.
    #[error("state store backend error: {source}")]
    Backend {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Encoding/decoding a persisted value failed (corrupt record / schema drift). Terminal.
    #[error("state (de)serialization failed: {source}")]
    Serialization {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The write carried a fence token lower than the highest committed for the scope —
    /// a superseded owner. Terminal: the caller must stop, not retry.
    #[error("write fenced: a newer owner holds this account")]
    Fenced,
    /// A blocking storage task (spawn_blocking) failed to join. Retryable.
    #[error("storage task failed: {0}")]
    Task(String),
}
```

- [ ] **Step 3: `cas_nonce_state` gains the fence** — in the `StateStore` trait, change the method and document the two guarantees:

```rust
    /// Store `state` iff the current version equals `expected_version` **and** `fence` is
    /// not below the highest fence committed for `scope`. On success bump the version and
    /// raise the stored fence to `max(stored, fence)`.
    ///
    /// Two independent guards: the **version** rejects lost updates (`Ok(false)` → retry);
    /// the **fence** rejects a superseded owner (`Err(Fenced)` → stop). In single-writer
    /// mode `fence` is always [`FenceToken::SINGLE_WRITER`], so the fence check is a no-op.
    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError>;
```
Add `FenceToken` to the `use crate::core::wallet::{…}` import at the top of `state_store.rs`.

- [ ] **Step 4: `InMemoryStateStore`** — track the fence. Change the field and impl in `nonce_store.rs`:

```rust
    // in the struct:
    nonces: Mutex<HashMap<NonceScope, (Versioned<NonceState>, FenceToken)>>,
```
```rust
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
            .map(|(v, f)| (v.version, *f))
            .unwrap_or((0, FenceToken::SINGLE_WRITER));
        if fence < cur_fence {
            return Err(StateStoreError::Fenced);
        }
        if cur_version != expected_version {
            return Ok(false);
        }
        nonces.insert(
            scope,
            (
                Versioned {
                    value: state.clone(),
                    version: expected_version + 1,
                },
                fence.max(cur_fence),
            ),
        );
        Ok(true)
    }
```
Add `FenceToken` to the `use crate::core::wallet::{…}` import in `nonce_store.rs`.

- [ ] **Step 5: `LocalNonceManager` carries a fence** — add a field and thread it in:

```rust
pub struct LocalNonceManager {
    store: Arc<dyn StateStore>,
    rpc: Arc<dyn Rpc>,
    fence: FenceToken,
}

impl LocalNonceManager {
    pub fn new(store: Arc<dyn StateStore>, rpc: Arc<dyn Rpc>) -> Self {
        // Single-writer-per-account is the documented default (SPEC §7); a distributed
        // lease issuer will supply a real fence in a later phase.
        Self { store, rpc, fence: FenceToken::SINGLE_WRITER }
    }
}
```
In `allocate`, `release`, `reset`, change every `self.store.cas_nonce_state(scope, version, &value).await?` to `self.store.cas_nonce_state(scope, version, &value, self.fence).await?`. `Err(Fenced)` propagates via `?` (never retried), surfacing as `NonceManagerError::Store(_)`.

- [ ] **Step 6: `MockStore`** (testutils) — update the two nonce methods' signatures; bodies stay `unreachable!` (nonce state is exercised via the real store):

```rust
    async fn cas_nonce_state(
        &self,
        _: NonceScope,
        _: u64,
        _: &NonceState,
        _: FenceToken,
    ) -> Result<bool, StateStoreError> {
        unreachable!("nonce state is exercised via the real InMemoryStateStore")
    }
```
Add `FenceToken` to testutils' `use crate::core::wallet::{…}` import.

- [ ] **Step 7: `error.rs` classification** — replace `store_kind` to match real variants:

```rust
fn store_kind(e: &StateStoreError) -> ErrorKind {
    match e {
        StateStoreError::Backend { .. } | StateStoreError::Task(_) => ErrorKind::Retryable,
        StateStoreError::Serialization { .. } | StateStoreError::Fenced => ErrorKind::Terminal,
    }
}
```

- [ ] **Step 8: serde derives + adapter accessors** — add `Serialize, Deserialize` to the derive lists of `TxHandle`, `TxStatus`, `HandleId` (`handle.rs`) and `GasEnvelope` (`policy.rs`); add `use serde::{Deserialize, Serialize};` to each file if absent. (`TxIntent`/`NonceState` already derive; alloy `Address`/`B256`/`Bytes`/`TxHash` serialize via the enabled `serde` feature.)

  The adapters key by raw bytes / store the fence as an integer, so add two `pub(crate)` accessors (the types stay opaque to external callers):
  - in `handle.rs`: `impl HandleId { pub(crate) fn as_bytes(self) -> [u8; 32] { self.0.0 } }` (the inner `B256`'s `.0` is a public `[u8; 32]`).
  - in `nonce.rs`: `impl FenceToken { pub(crate) fn as_u64(self) -> u64 { self.0 } pub(crate) fn from_u64(v: u64) -> Self { FenceToken(v) } }`.

- [ ] **Step 9: fix the direct CAS call site** in the `reset_retains_high_freed_nonce_and_drops_consumed_freed` test (`nonce_store.rs`): change `store.cas_nonce_state(scope, 0, &seeded)` → `store.cas_nonce_state(scope, 0, &seeded, FenceToken::SINGLE_WRITER)`.

- [ ] **Step 10: fence unit test** (in `nonce_store.rs` tests):

```rust
    #[tokio::test]
    async fn cas_rejects_a_lower_fence_and_raises_the_high_water() {
        let store = InMemoryStateStore::default();
        let scope = NonceScope::eoa(Address::ZERO);
        let s = NonceState::default();
        // Commit at a higher fence, then a lower fence is fenced out; equal/higher pass.
        assert!(store.cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER).await.unwrap());
        // A higher token commits and raises the high-water mark (uses the pub(crate)
        // `from_u64` added in Step 8).
        let higher = FenceToken::from_u64(1);
        assert!(store.cas_nonce_state(scope, 1, &s, higher).await.unwrap());
        // Now the sentinel (lower) is rejected as a superseded owner.
        assert!(matches!(
            store.cas_nonce_state(scope, 2, &s, FenceToken::SINGLE_WRITER).await,
            Err(StateStoreError::Fenced)
        ));
    }
```
`FenceToken` is in scope via `nonce_store.rs`'s `use crate::core::wallet::{…}` import (extended in Step 5).

- [ ] **Step 11: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo clippy --no-default-features && cargo test --all-targets`
Expected: all green — every existing test passes with `SINGLE_WRITER` threaded through (no behavior change); new fence test passes. Commit:
`git commit -m "feat(state): FenceToken seam on StateStore CAS + real StateStoreError variants + serde on handles"`

---

## Task 2: Backend-agnostic conformance suite

**Files:** Modify `src/testutils.rs` (add the suite), `src/adapters/nonce_store.rs` (run it against `InMemoryStateStore`).

**Interfaces produced:** `pub(crate) async fn state_store_conformance(store: Arc<dyn StateStore>)`.

- [ ] **Step 1: Write the suite** in `testutils.rs` — one function every backend must pass:

```rust
/// The contract every `StateStore` backend must satisfy. Run from each adapter's tests
/// so all backends behave identically (OpenRaft-style conformance).
pub(crate) async fn state_store_conformance(store: Arc<dyn StateStore>) {
    use crate::core::wallet::{FenceToken, NonceScope, NonceState, TxStatus};
    let account = Address::from([0x11; 20]);
    let scope = NonceScope::eoa(account);

    // --- nonce CAS: version conflict + commit ---
    let v0 = store.load_nonce_state(scope).await.unwrap();
    assert_eq!(v0.version, 0);
    let mut s = NonceState::default();
    s.next = 5;
    assert!(
        store.cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER).await.unwrap(),
        "first CAS commits"
    );
    // stale expected_version -> Ok(false), not an error.
    assert!(
        !store.cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER).await.unwrap(),
        "stale version is a conflict"
    );
    let v1 = store.load_nonce_state(scope).await.unwrap();
    assert_eq!(v1.version, 1);
    assert_eq!(v1.value.next, 5);

    // --- handle WAL: upsert / get / pending excludes terminal ---
    let sent = handle_for(account, 5, TxStatus::Sent);
    let done = handle_for(account, 6, TxStatus::Confirmed { block: 9 });
    store.put_handle(&sent).await.unwrap();
    store.put_handle(&done).await.unwrap();
    assert_eq!(store.handle(sent.id).await.unwrap().unwrap().nonce, 5);
    assert_eq!(
        store.handle(done.id).await.unwrap().unwrap().status,
        TxStatus::Confirmed { block: 9 }
    );
    let pending = store.pending_handles(account).await.unwrap();
    assert_eq!(pending.len(), 1, "terminal handle excluded from pending");
    assert_eq!(pending[0].nonce, 5);

    // --- upsert overwrites by id ---
    let mut sent2 = sent.clone();
    sent2.status = TxStatus::Confirmed { block: 12 };
    store.put_handle(&sent2).await.unwrap();
    assert!(store.pending_handles(account).await.unwrap().is_empty(), "now all terminal");
}
```
Add a small `handle_for(account, nonce, status)` fixture next to the existing `handle(nonce, status)` — same body but with an explicit account (so the suite can use a non-zero account):

```rust
pub(crate) fn handle_for(account: Address, nonce: u64, status: TxStatus) -> TxHandle {
    let mut h = handle(nonce, status);
    h.account = account;
    h.id = HandleId::new(h.intent_hash, nonce); // id depends on intent_hash+nonce, unchanged
    h
}
```
(The existing `handle()` uses `Address::ZERO`; the suite uses `0x11…` so it doesn't collide with other tests sharing a process-global backend like Postgres.)

- [ ] **Step 2: Run it against `InMemoryStateStore`** — add to `nonce_store.rs` tests:

```rust
    #[tokio::test]
    async fn in_memory_store_passes_conformance() {
        crate::testutils::state_store_conformance(Arc::new(InMemoryStateStore::default())).await;
    }
```

- [ ] **Step 3: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets`
Expected: green; `in_memory_store_passes_conformance` passes. Commit:
`git commit -m "test(state): backend-agnostic StateStore conformance suite (InMemory)"`

---

## Task 3: redb adapter (feature `redb`, default-on)

**Files:** Create `src/adapters/redb_store.rs`; modify `src/adapters/mod.rs`, `Cargo.toml`.

**Interfaces produced:** `RedbStateStore` with `pub fn open(path: impl AsRef<Path>) -> Result<Self, StateStoreError>`.

- [ ] **Step 1: Cargo** — add the optional dep + feature (and `tempfile` dev-dep):

```toml
[dependencies]
redb = { version = "4", optional = true }

[features]
default = ["tracing", "redb"]
redb = ["dep:redb"]

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write `redb_store.rs`.** Three tables; every method runs its redb transaction in `spawn_blocking` over an `Arc<Database>`; map redb errors to `StateStoreError::Backend`, serde errors to `Serialization`, and a join failure to `Task`. Nonce value is `serde_json` of `(version: u64, fence: u64, NonceState)`. Handle value is `serde_json(TxHandle)`. Keys: scope → account hex `String`; id → `[u8;32]` (`HandleId` bytes); pending index → `(String, [u8;32])`.

```rust
//! Durable embedded `StateStore` over redb (pure-Rust ACID KV). Sync redb runs inside
//! `spawn_blocking`; commits use `Durability::Immediate` (persist-before-broadcast).

use crate::core::deps::{StateStore, StateStoreError, Versioned};
use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
use crate::obs::debug;
use alloy_primitives::Address;
use async_trait::async_trait;
use redb::{Database, Durability, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;

// Ids are keyed as `&[u8]` (the 32 id bytes) to avoid depending on a redb `Key` impl for
// `[u8; N]`; `&[u8]`, `&str`, and tuples of them are first-class redb key types.
const NONCE: TableDefinition<&str, &[u8]> = TableDefinition::new("nonce");
const TX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tx");
const TX_PENDING: TableDefinition<(&str, &[u8]), ()> = TableDefinition::new("tx_pending");

pub struct RedbStateStore {
    db: Arc<Database>,
}

impl RedbStateStore {
    /// Open (or create) a redb database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateStoreError> {
        let db = Database::create(path).map_err(backend)?;
        // Create tables up front so first reads don't error on a missing table.
        let w = db.begin_write().map_err(backend)?;
        {
            w.open_table(NONCE).map_err(backend)?;
            w.open_table(TX).map_err(backend)?;
            w.open_table(TX_PENDING).map_err(backend)?;
        }
        w.commit().map_err(backend)?;
        Ok(Self { db: Arc::new(db) })
    }

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
    StateStoreError::Backend { source: Box::new(e) }
}
fn ser(e: serde_json::Error) -> StateStoreError {
    StateStoreError::Serialization { source: Box::new(e) }
}
fn scope_key(scope: &NonceScope) -> String {
    format!("{:x}", scope.account)
}
```
Then the `#[async_trait] impl StateStore for RedbStateStore` with each method calling `self.run(...)`. Key logic (write inside the closure with `Durability::Immediate`):

```rust
    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError> {
        let key = scope_key(&scope);
        let state = state.clone();
        self.run(move |db| {
            let mut w = db.begin_write().map_err(backend)?;
            w.set_durability(Durability::Immediate);
            let (cur_version, cur_fence): (u64, FenceToken) = {
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
            let bytes = serde_json::to_vec(&(expected_version + 1, fence.max(cur_fence), &state))
                .map_err(ser)?;
            {
                let mut t = w.open_table(NONCE).map_err(backend)?;
                t.insert(key.as_str(), bytes.as_slice()).map_err(backend)?;
            }
            w.commit().map_err(backend)?;
            Ok(true)
        })
        .await
    }
```
`load_nonce_state` decodes the same tuple (or `Versioned::default()` when absent). `put_handle` writes `TX[id]=serde_json(handle)` and, in the same write txn, `insert`/`remove` `TX_PENDING[(account,id)]` based on `handle.status.is_terminal()`. `pending_handles` opens a read txn and range-scans `TX_PENDING` over `(account, [0u8;32])..=(account, [0xff;32])`, loading each id from `TX`. `handle` reads `TX[id]`. Emit `debug!("redb …")` on writes.

> redb 4.x API note: confirm exact names (`begin_write`, `set_durability`, `open_table`, `Table::insert/get/remove`, `ReadableTable::range`, `WriteTransaction::commit`) against docs.rs/redb 4.x during implementation; the shapes above match the 4.x API but pin the version and adjust if a signature differs.

- [ ] **Step 3: Re-export** in `adapters/mod.rs`:

```rust
#[cfg(feature = "redb")]
mod redb_store;
#[cfg(feature = "redb")]
pub use redb_store::RedbStateStore;
```

- [ ] **Step 4: Conformance + a fence test** against redb (in `redb_store.rs` tests, `#[cfg(test)]`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn redb_store_passes_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStateStore::open(dir.path().join("wk.redb")).unwrap();
        crate::testutils::state_store_conformance(Arc::new(store)).await;
    }

    #[tokio::test]
    async fn redb_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wk.redb");
        let account = Address::from([0x22; 20]);
        let scope = NonceScope::eoa(account);
        {
            let store = RedbStateStore::open(&path).unwrap();
            let mut s = NonceState::default();
            s.next = 9;
            store.cas_nonce_state(scope, 0, &s, FenceToken::SINGLE_WRITER).await.unwrap();
        } // drop => close
        let store = RedbStateStore::open(&path).unwrap();
        assert_eq!(store.load_nonce_state(scope).await.unwrap().value.next, 9);
    }
}
```

- [ ] **Step 5: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo clippy --no-default-features && cargo test --all-targets`
Expected: `--no-default-features` builds (redb absent, adapter not compiled); default build runs `redb_store_passes_conformance` + `redb_persists_across_reopen`. Commit:
`git commit -m "feat(state): redb durable StateStore adapter (feature redb, default-on)"`

---

## Task 4: redb durable-restart recovery test

**Files:** Modify `src/adapters/redb_store.rs` (add a wallet-level restart test using mocks).

**Interfaces:** consumes the `Wallet` facade + testutils mocks + `RedbStateStore`.

- [ ] **Step 1: Write the restart test** — a `Wallet` over a redb file sends a tx (all mocks), the wallet is dropped (simulated crash), a fresh `Wallet` over the **same** redb path recovers and confirms it on one tick:

```rust
    #[tokio::test]
    async fn wallet_recovers_an_inflight_tx_after_restart_over_redb() {
        use crate::Wallet;
        use crate::testutils::{MockPolicy, MockRpc, MockSigner};
        use alloy_primitives::B256;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wk.redb");
        let h = B256::repeat_byte(1);
        // A chain view where the sent nonce (0) is mined and anchored -> confirmable.
        let rpc = || {
            Arc::new(MockRpc {
                tx_count: 1,
                block_number: 20,
                receipt: Some(crate::testutils::receipt(true, 8, h)),
                canonical: Some(h),
                ..Default::default()
            })
        };
        let build = |store: Arc<RedbStateStore>| {
            Wallet::builder(rpc(), Arc::new(MockSigner::default()), Arc::new(MockPolicy::default()))
                .confirmations(2)
                .store(store)
                .build()
        };

        // First instance: send, persisting a Sent handle to redb.
        let store1 = Arc::new(RedbStateStore::open(&path).unwrap());
        let id = {
            let w = build(store1.clone());
            w.send(&crate::testutils::intent()).await.expect("send").id
        }; // drop w + store1 => close db

        // Restart: fresh wallet over the SAME redb path recovers + confirms in one tick.
        let store2 = Arc::new(RedbStateStore::open(&path).unwrap());
        let w = build(store2);
        w.tick().await.expect("tick");
        assert!(matches!(
            w.status(id).await.expect("status"),
            Some(crate::core::wallet::TxStatus::Confirmed { .. })
        ));
    }
```
> If `Wallet::send` with `MockSigner`/`MockSubmit` defaults doesn't reach `Sent` in this wiring, mirror the exact mock setup the facade test / localnet restart test uses (`bump_timeout(0)` etc.); the point is a persisted non-terminal handle in redb before the "restart".

- [ ] **Step 2: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets`
Expected: the restart test passes — the tx confirms from redb-persisted state after reopen. Commit:
`git commit -m "test(state): wallet recovers an in-flight tx after restart over redb"`

---

## Task 5: Postgres adapter (feature `postgres`, default-off)

**Files:** Create `src/adapters/postgres_store.rs`; modify `src/adapters/mod.rs`, `Cargo.toml`.

**Interfaces produced:** `PostgresStateStore` with `pub async fn connect(url: &str) -> Result<Self, StateStoreError>`.

- [ ] **Step 1: Cargo** — optional `sqlx` + `postgres` feature:

```toml
[dependencies]
sqlx = { version = "0.8", optional = true, default-features = false, features = ["postgres", "runtime-tokio", "json"] }

[features]
postgres = ["dep:sqlx"]
```

- [ ] **Step 2: Write `postgres_store.rs`.** `PgPool`; `connect` runs idempotent DDL; CAS in a transaction with `SELECT … FOR UPDATE`; handle WAL via upsert/select. Store nonce `state` and handle as `serde_json::Value` (sqlx `json`), version/fence/nonce as `i64`.

```rust
//! Networked/shared `StateStore` over PostgreSQL via sqlx (pure-Rust driver). Suitable
//! for multiple replicas sharing per-account state; version-CAS gives cross-replica
//! gapless nonces, and the fence rejects a superseded owner (Phase-3 lease issuer).

use crate::core::deps::{StateStore, StateStoreError, Versioned};
use crate::core::wallet::{FenceToken, HandleId, NonceScope, NonceState, TxHandle};
use alloy_primitives::Address;
use async_trait::async_trait;
use sqlx::{PgPool, Row};

pub struct PostgresStateStore {
    pool: PgPool,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS nonce_state (
    account TEXT PRIMARY KEY, version BIGINT NOT NULL, fence BIGINT NOT NULL, state JSONB NOT NULL);
CREATE TABLE IF NOT EXISTS tx_handles (
    id BYTEA PRIMARY KEY, account TEXT NOT NULL, nonce BIGINT NOT NULL,
    terminal BOOLEAN NOT NULL, handle JSONB NOT NULL);
CREATE INDEX IF NOT EXISTS tx_handles_pending ON tx_handles(account) WHERE NOT terminal;
";

impl PostgresStateStore {
    pub async fn connect(url: &str) -> Result<Self, StateStoreError> {
        let pool = PgPool::connect(url).await.map_err(backend)?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(backend)?;
        Ok(Self { pool })
    }
}

fn backend<E: std::error::Error + Send + Sync + 'static>(e: E) -> StateStoreError {
    StateStoreError::Backend { source: Box::new(e) }
}
fn ser(e: serde_json::Error) -> StateStoreError {
    StateStoreError::Serialization { source: Box::new(e) }
}
```
CAS (transaction; distinguishes fence-out from version-conflict):

```rust
    async fn cas_nonce_state(
        &self,
        scope: NonceScope,
        expected_version: u64,
        state: &NonceState,
        fence: FenceToken,
    ) -> Result<bool, StateStoreError> {
        let account = format!("{:x}", scope.account);
        let fence_i = fence_to_i64(fence);
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let row = sqlx::query("SELECT version, fence FROM nonce_state WHERE account = $1 FOR UPDATE")
            .bind(&account)
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
        let (cur_version, cur_fence) = match &row {
            Some(r) => (r.get::<i64, _>("version"), r.get::<i64, _>("fence")),
            None => (0, fence_to_i64(FenceToken::SINGLE_WRITER)),
        };
        if fence_i < cur_fence {
            return Err(StateStoreError::Fenced);
        }
        if cur_version != expected_version as i64 {
            return Ok(false);
        }
        let json = serde_json::to_value(state).map_err(ser)?;
        sqlx::query(
            "INSERT INTO nonce_state (account, version, fence, state) VALUES ($1, $2, $3, $4)
             ON CONFLICT (account) DO UPDATE SET version = nonce_state.version + 1,
             fence = GREATEST(nonce_state.fence, $3), state = $4",
        )
        .bind(&account)
        .bind(expected_version as i64 + 1)
        .bind(fence_i.max(cur_fence))
        .bind(json)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(true)
    }
```
`load_nonce_state` selects version+state (`Versioned::default()` if absent). `put_handle` upserts `tx_handles` with `terminal = handle.status.is_terminal()`. `pending_handles` = `SELECT handle FROM tx_handles WHERE account = $1 AND NOT terminal`. `handle` selects by id (`id.as_bytes()` → `BYTEA`). Decode handles with `serde_json::from_value`. The token round-trip uses the `pub(crate)` `FenceToken::as_u64`/`from_u64` added in Task 1 Step 8; define a local `fn fence_to_i64(f: FenceToken) -> i64 { f.as_u64() as i64 }` in the adapter. (redb stores the token via serde and needs no such helper; Postgres stores it as an `i64`, so it does.)

- [ ] **Step 3: Re-export** in `adapters/mod.rs`:

```rust
#[cfg(feature = "postgres")]
mod postgres_store;
#[cfg(feature = "postgres")]
pub use postgres_store::PostgresStateStore;
```

- [ ] **Step 4: Conformance test, skipping without a DB** (in `postgres_store.rs`, `#[cfg(all(test, feature = "postgres"))]`):

```rust
#[cfg(all(test, feature = "postgres"))]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn postgres_store_passes_conformance() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let store = PostgresStateStore::connect(&url).await.expect("connect");
        // Isolate from other runs: the conformance suite uses account 0x11…; clear it.
        sqlx::query("DELETE FROM nonce_state WHERE account = $1")
            .bind(format!("{:x}", Address::from([0x11; 20])))
            .execute(&store.pool).await.unwrap();
        sqlx::query("DELETE FROM tx_handles WHERE account = $1")
            .bind(format!("{:x}", Address::from([0x11; 20])))
            .execute(&store.pool).await.unwrap();
        crate::testutils::state_store_conformance(Arc::new(store)).await;
    }
}
```

- [ ] **Step 5: Gate + report + commit on approval**

Run (no DB needed locally — the test skips): `cargo fmt --all --check && cargo clippy --all-targets --features postgres && cargo clippy --no-default-features && cargo test --all-targets --features postgres`
Expected: compiles with `postgres`; the conformance test prints the skip line without `DATABASE_URL`; `--no-default-features` still builds. Commit:
`git commit -m "feat(state): Postgres shared StateStore adapter (feature postgres, sqlx)"`

---

## Task 6: CI — run the Postgres tests

**Files:** Modify `.github/workflows/ci.yml`.

- [ ] **Step 1: Add a Postgres job** (keep the existing `gate` job as-is):

```yaml
  postgres:
    name: postgres backend tests
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env:
          POSTGRES_PASSWORD: postgres
          POSTGRES_DB: walletkit
        ports:
          - 5432:5432
        options: >-
          --health-cmd pg_isready --health-interval 10s --health-timeout 5s --health-retries 5
    env:
      DATABASE_URL: postgres://postgres:postgres@localhost:5432/walletkit
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - name: Clippy (postgres)
        run: cargo clippy --all-targets --features postgres -- -D warnings
      - name: Test (postgres)
        run: cargo test --all-targets --features postgres
```

- [ ] **Step 2: Add a `--no-default-features` build step** to the existing `gate` job (proves the trait + InMemory compile with no backends):

```yaml
      - name: Build (no default features)
        run: cargo build --no-default-features
```

- [ ] **Step 3: Gate + report + commit on approval**

Run locally: `cargo fmt --all --check && cargo clippy --all-targets && cargo build --no-default-features` (CI proves the Postgres path). Commit:
`git commit -m "ci: run Postgres backend tests + verify --no-default-features build"`

---

## Definition of done (sub-project B)

- `StateStore` is durable via **redb** (default-on) and **Postgres** (opt-in), both passing one conformance suite; `InMemoryStateStore` remains the zero-config default.
- The `FenceToken` seam is threaded through the CAS with reject-if-lower enforced (sentinel = no-op now); `NonceManager` public API unchanged; a Phase-3 lease issuer plugs in with no trait change.
- A wallet recovers an in-flight tx after restart over redb.
- Builds green with default features, `--features postgres`, and `--no-default-features`; CI runs the Postgres backend.
- Deferred (unchanged): distributed lease/fence **issuer**, Redis best-effort lease, NOOP gap-fill/cancel, intent-refill (D).
