# Facade (`Wallet`) + localnet integration harness — design

Status: **approved-in-brainstorm 2026-08-23**, ready for an implementation plan. This is
Task 18 (the composition root) plus the anvil-backed integration test phase that
exercises the whole stack — pipeline + executor + adapters — with real transactions.

## Goal

Give walletkit a small, ergonomic **public API** that wires the eight adapters into a
per-account runtime, then prove the system end-to-end against a real EVM (anvil): a
single tx, concurrent batches, an out-of-band tx stealing the nonce + recovery, fee
bumps, reorgs, and restart recovery. Even logic that unit tests already cover is
re-proven as an **emergent system property** (e.g. nonce management under real
concurrent submission), because in a live system the parts interact.

## Decisions (locked in brainstorm)

1. **Facade first, then the harness** — the harness wires through the facade and
   doubles as its integration test (no throwaway wiring; tests the real public API).
2. **Type name `Wallet`** (`walletkit::Wallet`), constructed by a **port-injecting
   builder** (`Wallet::builder(rpc, signer, policy)…build()`); the standard adapters
   for gas/nonce/store/submission/clock are built internally with override hooks. A
   high-level `connect(chain, url, key, policy)` convenience is deferred (YAGNI).
3. **Host-driven tick + opt-in spawner** — `tick()` runs one deterministic
   recover→confirm→escalate pass (time via the existing `Clock` port; no wall-clock, no
   sleep); `run(interval) -> LoopHandle` is opt-in sugar with explicit
   `LoopHandle::stop().await`. Research verdict: services own a background worker
   (thirdweb engine-core, OZ relayer, tx-sitter, Gelato); **libraries stay host-driven**
   — alloy's auto-heartbeat (#1318) and ethers' auto-spawned, silently-dying
   `GasEscalatorMiddleware` (deprecated) are the cautionary tales; the Rust async
   guidance is "a library must not own the runtime." A `Stream<TxEvent>` is the future
   step but needs the deferred `Emit`/`TxEvent` surface — not now.
4. **anvil embedded via `alloy-node-bindings`** — each test spawns `Anvil::new().spawn()`
   (funded accounts, auto-port, isolated chain — ideal for reorg/nonce isolation, and
   what alloy's own tests do). Tests **skip cleanly when the `anvil` binary is absent**,
   so `cargo test` stays green without Foundry.
5. **Full-sweep harness** — all eight scenarios in Component C in one plan.

## Component A — `Wallet` facade (composition root)

One `Wallet` instance = **one account** (the signer defines it), so
single-executor-per-account (matrix I4) is *structural*, not registry-enforced. A
multi-account registry is a later YAGNI add. Single-writer / exclusive-key-ownership is
the documented posture (see the nonce module docs) — creating two `Wallet`s for the same
key is unsupported.

**Builder** (`src/facade.rs`, re-exported from `lib.rs`):

```rust
Wallet::builder(rpc: Arc<dyn Rpc>, signer: Arc<dyn Signer>, policy: Arc<dyn PolicyEngine>)
    .confirmations(u64)     // default DEFAULT_REQUIRED_CONFIRMATIONS (12)
    .bump_timeout(u64)      // default DEFAULT_BUMP_TIMEOUT_SECS (30)
    .gas_ceiling(u128)      // RpcGasOracle max_fee ceiling (required — no sane default)
    .gas_buffer_pct(u128)   // default DEFAULT_GAS_BUFFER_PCT (25)
    .store(Arc<dyn StateStore>)   // override; default InMemoryStateStore
    .clock(Arc<dyn Clock>)        // override; default SystemClock
    .build() -> Result<Wallet, WalletError>
```

`build()` constructs, over `signer.address()`:
`RpcGasOracle(rpc, gas_ceiling)`, `LocalNonceManager(store, rpc)`, `PublicMempool(rpc)`,
then a `TransactionManager` and an `AccountExecutor` (both already guard `signer ==
account`; the executor `debug_assert`s it). Ports are shared via `Arc`.

**Public API:**

```rust
impl Wallet {
    fn account(&self) -> Address;
    async fn send(&self, intent: &TxIntent) -> Result<TxHandle, WalletError>;
    async fn status(&self, id: HandleId) -> Result<Option<TxStatus>, WalletError>; // via store
    async fn tick(&self) -> Result<(), WalletError>;          // one recover→confirm→escalate pass
    fn run(self: Arc<Self>, interval: Duration) -> LoopHandle; // opt-in spawner
}
```

- **`send` is just the pipeline — no handoff needed.** R4a made `executor.track()`
  *optional*: the executor works entirely off the persisted `TxHandle` (`recover`/
  `confirm`/`escalate` all read `pending_handles` from the store), and a cold approval
  cache simply re-evaluates policy from the persisted `intent` on the first bump. So
  `Wallet::send` runs `TransactionManager::send` and returns the handle — no approval
  surfaced, no tuple, no `track` seam, no "forget to track" footgun. (The approval cache
  stays a pure first-bump optimization; wiring it via the facade is deferred until
  there's a reason — it needs the pipeline to surface the approval, which it currently
  doesn't, and the re-eval fallback makes it unnecessary.)
- **`status(id)`** requires a **terminal-inclusive** store read — `pending_handles`
  excludes terminal handles, so a `Confirmed`/`Failed`/`Replaced` status can't be read
  through it. Add `StateStore::handle(&self, id: HandleId) -> Result<Option<TxHandle>,
  StateStoreError>` (implemented by `InMemoryStateStore`), consumed by `Wallet::status`
  and the harness assertions.
- **`tick`/`run`** per decision 3. `LoopHandle` holds the `JoinHandle` + a stop signal;
  `stop().await` aborts and joins; dropping it also stops (belt-and-suspenders), but
  `stop()` is the documented path.
- **`WalletError`** (`#[non_exhaustive]`, own type): `#[from] TransactionManagerError`,
  `#[from] ExecutorError`, plus a `Build` variant for construction failures.

*Testability:* the harness injects a `Transport` (over anvil) + `LocalSigner` (funded) +
an allow-all `PolicyEngine`, drives `tick()` manually at exact points, and asserts.

## Component B — localnet harness (anvil, embedded)

**Dev-deps (added at first use):** `alloy-node-bindings` (spawn anvil),
`alloy-provider` + `alloy-signer-local` (funded accounts / a second "external" signer) —
plus whatever the reverter deploy needs. No new production deps.

**`tests/support/mod.rs`:**
- `Localnet::spawn() -> Option<Localnet>` — `Anvil::new().spawn()`, build a `Wallet` over
  a `Transport` at the anvil endpoint funded from a dev account; **returns `None`
  (test logs + returns) when the `anvil` binary is absent** so the suite is a no-op
  without Foundry.
- helpers: `mine(n)`, `interval_mining(secs)` / `no_auto_mine()`, `reorg(depth)`
  (`anvil_reorg`), `snapshot`/`revert`, `steal_nonce(&signer)` (a **second signer on the
  same key**, or `anvil_impersonateAccount`, sends a 0-value self-transfer to consume the
  account's next nonce), and a minimal **reverting contract** (deploy once) for
  revert/`Failed` cases.

**`tests/localnet.rs`:** black-box integration tests through the `Wallet` public API,
ticks driven manually (deterministic; no sleeps). Skips when `spawn()` is `None`.

## Component C — scenarios (real txs, mapped to the matrix)

| # | Scenario | Asserts | Matrix |
|---|---|---|---|
| 1 | **Single tx**: `send` → `mine(1)` → `tick` → `Confirmed` | full e2e stack: build/sign/RLP accepted by node, receipt read, depth→terminal | C1, I1 |
| 2 | **Revert**: send to the reverter → `SimulationRejected` at estimate; and a force-past-estimate path → `Failed` at depth | pre-sign gate + reverted-receipt classification | H1, C2 |
| 3 | **Concurrent batch**: N `send` on one account (join set) → distinct **gapless** nonces, all mine, all `Confirmed` | nonce management as a *system* under real concurrent submission | A1, I1 |
| 4 | **External nonce steal + recover**: `steal_nonce` consumes the next nonce, then `send` → `nonce too low` → assume-sent (nonce kept) → `tick` → `Replaced`; allocator reconciles forward; a following `send` mines | out-of-band + recovery as a system | G1, A5, A8, G2 |
| 5 | **Stuck-tx bump**: `no_auto_mine`, low-fee `send`, `bump_timeout(0)`, `tick` `escalate` → same-nonce RBF accepted → `mine` → `Confirmed`; multi-round fee grows monotonically | real RBF acceptance + escalation | B1, B5, B7 |
| 6 | **Reorg un-mine**: mine our tx, `reorg(depth)` past it → `tick` → back to `Sent`/re-tracked; a stale-receipt variant → `Unknown` (no false transition) | real reorg mechanics + the hash-anchor crux | D1, D3, D5 |
| 7 | **Restart recovery**: drop the executor, rebuild a `Wallet` over the **same store**, `tick` → rebroadcasts in-flight / reconciles-to-Confirmed if it mined during downtime; nonce re-syncs to `max(persisted, chain)` | crash-recovery path | F1, F2, F3 |
| 8 | **Invariant sweeps** (asserted across 1–7): I1 exactly-once (one hash mines per intent+nonce), I3 every handle reaches a terminal/trackable state — never hangs | system invariants | I1, I3 |

## Testing / gating

- Integration tests live in `tests/` (black-box, public API only) — separate from the
  `#[cfg(test)]` unit path. They **skip without Foundry**, so `cargo fmt --check` +
  `cargo clippy --all-targets` + `cargo test` stay green everywhere; with Foundry
  installed, the full localnet sweep runs.
- Unit tests (pure `transition` table + shell) remain the fast, dependency-free layer;
  the harness is the slow, high-fidelity layer. Both are kept.

## Non-goals / deferred

- Multi-account registry on one `Wallet` (Phase 2 — many instances suffice now).
- `Stream<TxEvent>` / `Emit` observability surface (needs the deferred event type).
- High-level `connect(chain, url, key, policy)` convenience (add at a real consumer).
- Durable `StateStore` (Phase 3) — the harness's restart test uses the same in-memory
  store instance to stand in for durability.
- The remaining matrix rows without a scenario here (D4/D6/D7 deep-reorg variants, E/G
  NOOP/cancel/refill) stay unit-or-Phase-2/3 per the test-matrix doc.

## References

- Loop-driving model (services vs libraries): thirdweb engine-core, OZ
  openzeppelin-relayer, worldcoin tx-sitter, Gelato (background workers); alloy
  `heart.rs` + [#1318](https://github.com/alloy-rs/alloy/issues/1318), ethers-rs
  `GasEscalatorMiddleware` (deprecated, drop-kills-task), viem per-call poller;
  [Rust async-book — ecosystem](https://rust-lang.github.io/async-book/08_ecosystem/00_chapter.html)
  ("a library must not own the runtime").
- Scenario provenance: `docs/plans/2026-08-22-executor-test-matrix.md` (A–I) and the
  concurrency matrix in `docs/plans/2026-08-22-executor-refactor.md` (#1–23).
- anvil test-node: `alloy-node-bindings` (`Anvil`), `anvil_reorg` / `anvil_impersonateAccount`.
