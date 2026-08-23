# Sub-project B — Durable State & Recovery (design)

**Status:** approved 2026-08-23 · **Phase:** 1 robustness (B of A–F, +G) · **Depends on:** A (errors + observability), on `main`.
**Spec basis:** SPEC.md §3/§4 (locked decision 10: embedded redb/SQLite default), §7 (distributed-nonce fencing seam ships Phase 1, distributed impl deferred). Research: 4 cited agents (embedded KV · networked store · fencing cross-domain · pluggable-backend abstraction).

## Goal

Make walletkit's `StateStore` durable so transaction tracking survives process restarts, and reserve the single-writer **fencing seam** so a distributed deployment is an additive change later. Ships two backends behind the existing port — an embedded one (redb) and a networked/shared one (Postgres) — plus a backend-agnostic conformance suite.

## Scope

**In:** `FenceToken` seam threaded through the `StateStore` nonce CAS (single-writer sentinel now); real `StateStoreError` variants; `serde` on the handle types; a **redb** embedded adapter (feature `redb`, default-on); a **Postgres** networked adapter (feature `postgres`, default-off); a backend-agnostic **conformance test suite**; a durable-restart recovery test; CI wiring for the Postgres tests.

**Out (deferred):** the distributed **lease/fence *issuer*** (etcd/advisory-lock leadership epochs that mint real monotonic tokens on failover) — Phase 3 per SPEC §7; Redis/Valkey as a store (rejected — unsafe as a crash-safe authority; only ever an optional best-effort lease/queue later); NOOP gap-fill/cancel, intent-refill (sub-project D).

## Locked decisions (with the research behind them)

1. **Embedded = redb v4.x.** Pure-Rust (no C toolchain — decisive for an embedded library), ACID/crash-safe by default (`Durability::Immediate` = persist-before-broadcast), right-sized for a tiny KV+CAS WAL. RocksDB/LevelDB rejected (write-heavy C++/legacy LSMs, all build-cost no benefit here); SQLite = boring fallback but relational-overkill + C dep; LMDB map-size footgun; sled/fjall durability footguns.
2. **Networked = PostgreSQL, not Redis.** Redis is unsafe as the crash-safe authority (Kleppmann/Jepsen: no fencing tokens; AOF `everysec` can lose ~1s of acked writes). Postgres is what tx-sitter uses (atomic `UPDATE … RETURNING` nonce; a version column is the fence). `sqlx` gives a pure-Rust Postgres driver (no C). etcd/Dynamo/FDB reserved as future adapters behind the same port.
3. **Fencing = opaque monotonic `FenceToken` on the `StateStore` CAS**, `SINGLE_WRITER` sentinel default, **reject-if-lower enforced from day one** (a no-op under the sentinel). Enforcement lives at the resource (the store), per Kleppmann; opaque per Chubby's sequencer; the model is Kafka epochs / etcd revisions. `NonceManager` public signatures unchanged.
4. **Port shape follows Apache `object_store`**: object-safe `#[async_trait]` behind `Arc<dyn>`, portable version token (`u64`), one error enum with a boxed `Backend { source }` catch-all + a `Task` (spawn_blocking join) variant; **additive** feature-gated backends; `--no-default-features` builds trait + `InMemory`. Ship an **OpenRaft-style conformance suite** run against every backend.
5. **Recovery = no boot step.** A durable store makes the existing per-tick `recover()` + `confirm()` reconcile idempotently on the first tick after restart (`reset(account, chain_tx_count)` gives `next = max(persisted, chain)`; persist-before-broadcast means no on-chain-but-not-in-store handle → no double-spend).

---

## Part 1 — Port & primitives

### `FenceToken` (new, in `core/wallet/primitives/nonce.rs`)
```
/// Opaque, monotonic ownership token for the single-writer nonce seam. The store
/// records the highest token it has committed per scope and rejects any lower one
/// (Kleppmann fencing; enforced at the resource). Phase 1 uses SINGLE_WRITER only;
/// a Phase-3 lease issuer mints real tokens with no trait change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FenceToken(u64);
impl FenceToken {
    pub const SINGLE_WRITER: FenceToken = FenceToken(0);
}
```

### `StateStore` CAS gains the fence (`core/deps/state_store.rs`)
```
async fn cas_nonce_state(
    &self,
    scope: NonceScope,
    expected_version: u64,
    state: &NonceState,
    fence: FenceToken,
) -> Result<bool, StateStoreError>;
```
Semantics (documented on the trait): `Ok(true)` committed; `Ok(false)` **version conflict** → caller retries (lost-update protection); `Err(StateStoreError::Fenced)` **token < highest committed for this scope** → fatal, do not retry (stale-writer protection). The store persists `(version, fence_high, NonceState)` per scope; commit writes `(version+1, max(fence_high, fence), new_state)`. `load_nonce_state` still returns `Versioned<NonceState>` (the fence is store-enforced, not caller-read).

### `StateStoreError` (real variants)
```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateStoreError {
    #[error("state store backend error: {source}")]
    Backend { source: Box<dyn std::error::Error + Send + Sync> },
    #[error("state (de)serialization failed: {source}")]
    Serialization { source: Box<dyn std::error::Error + Send + Sync> },
    #[error("write fenced: a newer owner holds this account")]
    Fenced,
    #[error("storage task failed: {0}")]
    Task(String),
}
```
`WalletKitError` classification (update `error.rs` `store_kind`, no longer uninhabited): `Backend`/`Task` → `Retryable`; `Serialization` → `Terminal`; `Fenced` → `Terminal` (a fenced writer must stop, not retry).

### `NonceManager` (unchanged public API)
`allocate`/`release`/`reset` signatures stay identical. `LocalNonceManager` gains a `fence: FenceToken` field (constructed default `SINGLE_WRITER`) threaded into its `cas_nonce_state` calls; `Err(Fenced)` propagates out of the CAS loop (never retried).

### Serialization
Add `#[derive(Serialize, Deserialize)]` to `TxHandle`, `TxStatus`, `HandleId`, `GasEnvelope` (TxIntent/NonceState already derive; alloy `Bytes`/`TxHash`/`Address`/`B256` via the enabled serde feature). Codec: **`serde_json`** (already a dep; debuggable; volume is tiny).

### In-memory + mock
`InMemoryStateStore` and `MockStore` (testutils) implement the new CAS signature and honor the fence (track `fence_high`, reject lower) so they pass the same conformance suite.

---

## Part 2 — Adapters

### `RedbStateStore` — `src/adapters/redb_store.rs` (feature `redb`, default-on)
`Arc<redb::Database>`; every `StateStore` method runs the redb transaction inside `tokio::task::spawn_blocking` (redb is sync; join errors → `StateStoreError::Task`). Tables:
- `NONCE: TableDefinition<&str /*scope*/, &[u8] /*serde_json((version,u64), (fence_high,u64), NonceState)*/>`
- `TX: TableDefinition<[u8;32] /*id*/, &[u8] /*serde_json(TxHandle)*/>`
- `TX_PENDING: TableDefinition<(&str /*account*/, [u8;32] /*id*/), ()>` — index maintained in the *same* write txn (insert when non-terminal, remove when terminal) so record+index are atomic.

CAS = one write txn: read current; `fence < fence_high` → abort + `Err(Fenced)`; `expected_version != version` → abort + `Ok(false)`; else insert bumped tuple + `commit()` (`Durability::Immediate`). `pending_handles` = range scan over `TX_PENDING` for the account. `open(path) -> Result<Self, StateStoreError>`.

### `PostgresStateStore` — `src/adapters/postgres_store.rs` (feature `postgres`, default-off)
`sqlx::PgPool` (pure-Rust driver). Schema created idempotently on `connect(url)` via `CREATE TABLE IF NOT EXISTS` (no migration framework — YAGNI):
```
nonce_state(account TEXT PRIMARY KEY, version BIGINT NOT NULL, fence BIGINT NOT NULL, state JSONB NOT NULL)
tx_handles(id BYTEA PRIMARY KEY, account TEXT NOT NULL, nonce BIGINT NOT NULL, terminal BOOL NOT NULL, handle JSONB NOT NULL)
CREATE INDEX ... ON tx_handles(account) WHERE NOT terminal
```
CAS in one tx: `SELECT version, fence FROM nonce_state WHERE account=$1 FOR UPDATE`; if `fence > $fence` → `Err(Fenced)`; if `version != $expected` → `Ok(false)`; else `INSERT ... ON CONFLICT (account) DO UPDATE SET version=version+1, fence=GREATEST(fence,$fence), state=$state` → `Ok(true)`. `pending_handles` = `SELECT handle FROM tx_handles WHERE account=$1 AND NOT terminal`. Errors map to `Backend { source }`.

### Wiring
No new builder method — hosts pass `Arc::new(RedbStateStore::open(path)?)` or `Arc::new(PostgresStateStore::connect(url).await?)` to the existing `WalletBuilder::store(...)`. `InMemoryStateStore` remains the zero-config default (no files/connections unless the host opts in).

---

## Part 3 — Recovery

No code change. New tests prove it: build a `Wallet` over a redb file, `send`, drop it (simulated crash), rebuild a fresh `Wallet` over the **same** redb path, `tick()` → the in-flight handle reconciles/confirms from persisted state. (The localnet harness already does this over `InMemoryStateStore` in one process; B adds the cross-instance durable version.)

## Part 4 — Testing (every test earns its place)

- **Conformance suite** (`src/testutils.rs`, `pub(crate)`): `async fn state_store_conformance(store: Arc<dyn StateStore>)` asserting the contract: nonce CAS commit/`version`-conflict-retry; **fence reject-if-lower** (`SINGLE_WRITER` accepted, a lower token after a higher one → `Fenced`); handle upsert/get; `pending_handles` excludes terminal; durability round-trip where applicable. Run from unit tests against `InMemoryStateStore` and `RedbStateStore` (tempfile), and against `PostgresStateStore` in a `#[cfg(all(test, feature = "postgres"))]` test that **skips when `DATABASE_URL` is unset** (mirrors localnet skipping without anvil).
- **Fence unit test**: reject-if-lower + `max(fence_high, fence)` monotonicity.
- **Durable restart test**: the redb cross-instance recovery above.
- Not tested: serde derive mechanics, schema DDL.

## Part 5 — Deps, features, CI

```toml
[dependencies]
redb = { version = "4", optional = true }
sqlx = { version = "0.8", optional = true, default-features = false, features = ["postgres", "runtime-tokio", "json"] }

[features]
default = ["tracing", "redb"]
redb = ["dep:redb"]
postgres = ["dep:sqlx"]

[dev-dependencies]
tempfile = "3"
```
CI: add `--features postgres` to a job with a Postgres service (`DATABASE_URL` set) so the Postgres conformance test runs; keep the existing `--no-default-features` build green (trait + InMemory only). (Making cargo-deny/cargo-hack required stays in G.)

---

## File-by-file

| File | Change |
|---|---|
| `src/core/wallet/primitives/nonce.rs` | add `FenceToken` (+ re-export) |
| `src/core/deps/state_store.rs` | `cas_nonce_state` gains `fence`; real `StateStoreError` variants; doc the two guarantees |
| `src/core/wallet/primitives/{handle,policy}.rs` | `serde` derives on `TxHandle`/`TxStatus`/`HandleId`/`GasEnvelope` |
| `src/adapters/nonce_store.rs` | `InMemoryStateStore` fence handling; `LocalNonceManager` fence field + threading |
| `src/error.rs` | `store_kind` matches real `StateStoreError` variants |
| `src/adapters/redb_store.rs` (new) | redb adapter (feature `redb`) |
| `src/adapters/postgres_store.rs` (new) | Postgres adapter (feature `postgres`) |
| `src/adapters/mod.rs` | feature-gated re-exports |
| `src/testutils.rs` | `MockStore` new CAS; `state_store_conformance` suite |
| `Cargo.toml` | redb/sqlx optional deps + features + tempfile dev-dep |
| `.github/workflows/ci.yml` | Postgres service job for `--features postgres` |

## Task breakdown (for writing-plans)

1. **Port + fence + serde** — `FenceToken`, `StateStoreError` variants, `cas_nonce_state(+fence)`, update `InMemoryStateStore`/`MockStore`/`LocalNonceManager`, `error.rs` classification, serde derives. Green with sentinel fence (all existing tests pass).
2. **Conformance suite** — `state_store_conformance` in testutils; run against `InMemoryStateStore`; fence reject-if-lower unit test.
3. **redb adapter** (feature `redb`, default-on) — implement + conformance test (tempfile) + `--no-default-features` still builds.
4. **redb durable-restart recovery test** — cross-instance rebuild over one redb path.
5. **Postgres adapter** (feature `postgres`) — schema-on-connect + CAS/fence + WAL queries; conformance test skipping without `DATABASE_URL`.
6. **CI** — Postgres service job + `--features postgres`; keep `--no-default-features` green.

Each task ends green under `fmt --check` + `clippy --all-targets` (+ `--no-default-features` where relevant) + `test`, committed on approval per CLAUDE.md, following the A-phase observability + error standards.
