# Sub-project A — Errors + Observability (design)

**Status:** approved 2026-08-23 · **Branch:** `feat/observability-errors` · **Phase:** 1 robustness (A of A–F, +G)
**Spec basis:** SPEC.md §5.5 (unified error taxonomy) + §5.6 (observability). Research: 3 agents (Rust library logging standards · event-surface patterns · dashboards) — all cited in-session.

## Goal

Give walletkit the two cross-cutting §5 must-haves it currently lacks: (1) **one public error type** with machine-readable classification, retry semantics, and remediation; (2) **`tracing` instrumentation** to library-grade industry standard — intent-hash-correlated spans, structured fields, disciplined levels, and hard redaction on key paths. Both are foundational, so they land first (every later sub-project emits them from birth).

## Scope

**In:** the unified `WalletKitError` taxonomy; `tracing` spans/events across the pipeline, executor, and adapters; redaction discipline; classification/redaction tests.

**Out (deferred, by decision):** the typed `LifecycleEvent`/`Observer`/`Stream` programmatic event API (fold into sub-project D if a consumer needs it — lifecycle transitions are emitted as `tracing` events here). Metrics facade, OpenTelemetry/OTLP export, and dashboards (host/companion-crate concern; the library emits plain `tracing` and a host bridges via `tracing-opentelemetry`). No subscriber is ever installed by the library.

## Locked decisions

1. **Flatten `WalletKitError` by domain** (not by subsystem). Per-port `{Trait}Error` enums are unchanged (port contracts); `WalletKitError` is the public umbrella.
2. **`tracing` is an optional dependency behind a default-on feature named `tracing`** (with the tracing `log` feature enabled). Instrumentation is cfg-gated so `default-features = false` compiles to no-ops. Never install a subscriber / `set_global_default` / depend on `opentelemetry` in core.
3. **`retry_after()` returns `None` in Phase 1** — the method exists (satisfies §5.5) but a real value waits until the Transport surfaces server `Retry-After`/rate-limit hints (sub-project E). `is_retryable()` is the live signal.

---

## Part 1 — Unified error taxonomy (§5.5)

### Placement & migration
- New module **`src/error.rs`**, re-exported from `lib.rs` as the public error surface.
- **Rename** the facade's `WalletError` → `WalletKitError` and move it here. Update `facade.rs` (`send`/`status`/`handle`/`tick`/`run`) and `WalletBuilder` signatures to return `WalletKitError`.
- The service-layer enums `TransactionManagerError` and `ExecutorError` stay (they aggregate port errors internally); `WalletKitError` provides `From` for both, flattening their overlapping variants.

### Types
```
pub enum ErrorKind { Retryable, Terminal, NeedsReconcile }   // machine-readable classification

#[non_exhaustive]
pub enum WalletKitError {
    Rpc(RpcError),
    Signer(SignerError),
    Policy(PolicyRejection),          // denied — names the exact rule + field
    PolicyEngine(PolicyEngineError),  // operational policy failure (load/eval)
    Gas(GasOracleError),
    Nonce(NonceManagerError),
    Submission(SubmissionError),
    Store(StateStoreError),
    Simulation { reason: String },    // estimate_gas revert (would-revert gate)
    AccountMismatch { intent: Address, signer: Address },
}
```

### Public methods
- `kind(&self) -> ErrorKind`
- `is_retryable(&self) -> bool` — `self.kind() == Retryable`
- `retry_after(&self) -> Option<Duration>` — `None` in Phase 1 (seam; see decision 3)
- `remediation(&self) -> Option<&'static str>`
- `policy_rejection(&self) -> Option<&PolicyRejection>` — `Some` only for `Policy`

### Classification table (the regression-worthy logic)
| Variant / condition | `ErrorKind` | Remediation hint (example) |
|---|---|---|
| `Rpc(Call{transient:true})` | Retryable | "transient RPC error — retry" |
| `Rpc(Call{transient:false})` | Terminal | "RPC rejected the call — inspect the request" |
| `Submission` where `is_transient()` | Retryable | "submission indeterminate — retry" |
| `Submission` where `is_already_accepted()` | NeedsReconcile | "already in the mempool — let the executor reconcile" |
| `Submission` other | Terminal | "submission rejected — the tx will not broadcast" |
| `Signer(ApprovalMismatch/Expired/FeesExceedApproval)` | Terminal | "re-submit the intent for a fresh approval" |
| `Signer(Load)` | Terminal | "check the key backend configuration" |
| `Signer(Backend)` | Terminal | "signing backend failed" (Retryable once remote signers land, Phase 4) |
| `Policy(_)` (denied) | Terminal | rejection's own reason (rule + field) |
| `PolicyEngine(Load/Eval)` | Terminal | "policy engine failed to load/evaluate" |
| `Gas(CeilingExceeded)` | Terminal | "raise gas_ceiling or wait for base fee to fall" |
| `Gas(Rpc)` / `Nonce(Rpc)` | (delegate to `Rpc` rule) | — |
| `Nonce(Store)` / `Store(_)` | Retryable | "state store I/O — retry" (empty enum today; real once sub-project B lands) |
| `Simulation` | Terminal | "the transaction would revert — inspect calldata/state" |
| `AccountMismatch` | Terminal | "sign with the key that controls the intent account" |

Classification is derived from signal already carried by the port errors (`RpcError.transient`, `SubmissionError::is_transient/is_already_accepted`) — no new fields on port errors.

---

## Part 2 — `tracing` instrumentation (§5.6)

### Dependency
`Cargo.toml`: `tracing = { version = "0.1", optional = true, default-features = false, features = ["std", "log"] }`; `[features] tracing = ["dep:tracing"]`; add `tracing` to `default`. A small internal shim keeps call sites clean and no-ops when the feature is off (so `#[instrument]`/`info!` sites don't sprawl `cfg`).

### Span tree (correlation by intent hash)
- **Root:** `TransactionManager::send` opens `wallet.send { intent_hash, account, chain_id }`. All stage spans/events inherit it.
- **Pipeline stages** (children, DEBUG spans): `estimate`, `policy`, `allocate`, `sign` (**`skip_all`**), `submit`.
- **Executor:** `AccountExecutor::tick` span; per-handle work in `confirm`/`escalate`/`bump` wrapped in a span carrying that handle's `intent_hash` (+ `nonce`) so background transitions correlate across the async boundary. `bump` is `skip_all`.

### Field & level conventions
- **Fields (snake_case):** `intent_hash`, `account`, `chain_id`, `nonce`, `gas_limit`, `max_fee`, `max_priority_fee`, `tx_hash`, `block`, `status`, `kind`, `bump_count`.
- **Levels:** ERROR = terminal failure surfaced to the caller · WARN = recoverable/degraded (gas bump, reorg detected, replacement observed, RPC failover/retry) · INFO = sparse lifecycle milestones (tx submitted, tx confirmed) — ~1 per tx, never per-poll · DEBUG = mechanics (nonce assigned, per-attempt fees, poll iteration) · TRACE = FSM transitions, raw shapes.
- **Lifecycle events:** each status transition in `confirm` emits a `tracing` event `{ intent_hash, from, to, block?, tx_hash? }` at INFO (terminal) / WARN (reorg/replaced) — this is where §5.6 "structured lifecycle events" lands.

### Redaction (the #1 risk — we handle keys)
- **Never** `#[instrument]` a fn taking key material, `PolicyApproval`, `TxEip1559`, or signed bytes without `skip_all`.
- Signing paths use `#[instrument(skip_all, fields(<safe-only>))]` — an **allow-list** (a later-added secret arg silently leaks under `skip(...)` but stays safe under `skip_all`).
- Secret-bearing types get a redacting `Debug` (`"[redacted]"`); verify `LocalSigner`/key wrappers never derive a key-exposing `Debug`.
- Do **not** enable tracing's `log-always` (removes host opt-out).

---

## Data flow / correlation

`intent.hash()` is computed at `send` entry → set as the `intent_hash` span field on the root; child spans inherit via tracing's context propagation (including `.instrument()`-wrapped async). The executor, running on its own tick cadence, re-establishes a per-handle span from the persisted `TxHandle.intent_hash`, so a tx's send-time and tracking-time telemetry share the same correlation id even though they occur in different tasks. A host that adds `tracing-opentelemetry` gets a unified OTel trace per intent for free.

## Error handling within A

Purely additive: tracing macros are infallible; classification is pure. No new fallible paths, no panics. `WalletKitError` construction is total.

## Testing (every test earns its place)

- **Classification tests** (logic that can regress): for a representative error of each `ErrorKind` bucket, assert `kind()`, `is_retryable()`, and `policy_rejection()`; assert `From<TransactionManagerError>`/`From<ExecutorError>` map to the right flat variant and preserve the source.
- **Redaction test (security invariant):** a capturing `tracing-subscriber` layer (dev-dep) runs a `sign`/`bump` path and asserts the captured span/event fields contain **only** the allow-listed names and **never** a key/approval/signed-byte value.
- **Not tested:** span/event plumbing (glue), field formatting.
- **Dev-deps:** `tracing-subscriber` (capture); optionally `tracing-test`.

## File-by-file changes

| File | Change |
|---|---|
| `src/error.rs` (new) | `ErrorKind`, `WalletKitError`, classification/remediation, `From` impls |
| `src/lib.rs` | `pub mod error; pub use error::{WalletKitError, ErrorKind};` |
| `src/facade.rs` | remove local `WalletError`; return `WalletKitError`; instrument `send`/`tick` |
| `src/core/wallet/transaction_manager.rs` | root `wallet.send` span + stage events; `skip_all` on sign |
| `src/core/wallet/executor/mod.rs` | `tick`/`confirm`/`escalate`/`bump` spans; transition events; `skip_all` on `bump` |
| `src/core/wallet/signing.rs` | `skip_all` on sign/encode |
| `src/adapters/{transport,nonce_store,gas_oracle,public_mempool,signers}.rs` | DEBUG spans/events on I/O; verify no secret in `Debug` |
| `Cargo.toml` | `tracing` optional dep + `tracing` default-on feature; dev-dep `tracing-subscriber` |

## Task breakdown (for writing-plans)

1. **Error module** — `WalletKitError` + `ErrorKind` + classification + remediation + `From` impls; rename/migrate `WalletError`; update facade signatures. Classification unit tests.
2. **tracing dep + shim** — add the optional dep/feature and the no-op-when-off shim; wire `lib.rs`.
3. **Core instrumentation** — root span in `send` + stage events; executor `tick`/`confirm`/`escalate`/`bump` spans + transition events; `skip_all` redaction on all signing paths.
4. **Adapter instrumentation** — transport/nonce/gas/submission DEBUG spans; secret-`Debug` audit.
5. **Redaction test** — capturing-subscriber test asserting no secret in sign/bump telemetry.

Each task ends green under the gate (`fmt --check` + `clippy --all-targets` + `test`) and is committed only on approval, per CLAUDE.md.
