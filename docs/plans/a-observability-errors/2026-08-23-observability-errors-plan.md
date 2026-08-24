# Sub-project A — Errors + Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give walletkit one public error taxonomy (`WalletKitError` with machine-readable classification, §5.5) and library-grade `tracing` instrumentation (intent-hash-correlated spans, disciplined levels, hard redaction on key paths, §5.6).

**Architecture:** Per-port `{Trait}Error` enums stay as-is; a new `src/error.rs` provides the public `WalletKitError` umbrella that `From`-maps and classifies them. `tracing` is an optional dep behind a default-on feature named `tracing`; a thin `src/obs.rs` shim re-exports the event macros (or no-ops them when the feature is off) so call sites carry no `cfg`, and function spans use `#[cfg_attr(feature = "tracing", tracing::instrument(...))]`.

**Tech Stack:** Rust edition 2024, `tracing` 0.1 (optional), `tracing-subscriber` + `tracing-test`? (dev, for the redaction test — this plan uses a small custom capture layer via `tracing-subscriber` only).

## Global Constraints

- **Review-gated workflow (CLAUDE.md):** each task — write the whole task's code, run `cargo fmt --all --check` + `cargo clippy --all-targets` + `cargo test --all-targets`, report the real output, leave **uncommitted**, commit **only on explicit approval**. Commit messages have **no** `Co-Authored-By` trailer.
- **No `unwrap()`/`expect()`/`panic!` in production code** — allowed only in `#[cfg(test)]`, `const`, or a documented infallible invariant. Prefer `parking_lot`.
- **Hexagonal:** ports one-per-file with `{TraitName}Error`; `core/*` zero-I/O; adapters implement ports.
- **`tracing`:** optional dep, default-on feature named `tracing`; **never** install a subscriber, call `set_global_default`, or depend on `opentelemetry` in the core crate. Do not enable tracing's `log-always`.
- **Redaction:** every signing/key path uses `#[instrument(skip_all, fields(<allow-list>))]`. No key material, `PolicyApproval`, `TxEip1559`, or signed bytes ever becomes a span/event field.
- **Levels:** ERROR = terminal caller-facing failure · WARN = recoverable/degraded (bump, reorg, replaced, failover, retry) · INFO = sparse milestones (submitted, confirmed) · DEBUG = mechanics · TRACE = FSM transitions.
- **Testing:** only regression-worthy tests (classification logic, redaction invariant). No span-plumbing tests. DRY, YAGNI.
- MSRV `rust-version = "1.85"`, edition 2024.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/error.rs` (new) | `ErrorKind`, `WalletKitError`, classification/remediation, `From<TransactionManagerError/ExecutorError/StateStoreError>`, classification tests |
| `src/obs.rs` (new) | Event-macro shim (`info!/warn!/error!/debug!/trace!`) — re-export when feature on, no-op when off |
| `src/lib.rs` | add `pub mod error;` + `pub(crate) mod obs;`; export `WalletKitError`/`ErrorKind`; drop `WalletError` |
| `src/facade.rs` | remove local `WalletError`; methods return `WalletKitError`; instrument `send`/`tick` (thin) |
| `src/core/wallet/transaction_manager.rs` | root `wallet.send` span + stage events; `skip_all` on sign call |
| `src/core/wallet/executor/mod.rs` | `tick`/`bump` spans; per-transition + bump events |
| `src/core/wallet/signing.rs` | `skip_all` instrument on `sign_encode` |
| `src/adapters/{nonce_store,gas_oracle,public_mempool,signers}.rs` | DEBUG mechanics events; secret-`Debug` audit |
| `tests/localnet.rs` | update the flattened error match (`WalletKitError::Submission(_)`) |
| `Cargo.toml` | `tracing` optional dep + `tracing` default-on feature; dev-dep `tracing-subscriber` |

---

## Task 1: Unified error taxonomy (`WalletKitError`)

**Files:**
- Create: `src/error.rs`
- Modify: `src/lib.rs:14`, `src/facade.rs:222-233` (remove `WalletError`), `src/facade.rs` imports + method return types, `tests/localnet.rs:69-90` (flattened match)

**Interfaces:**
- Produces: `pub enum ErrorKind { Retryable, Terminal, NeedsReconcile }`; `pub enum WalletKitError` (flat domain variants); `WalletKitError::{kind, is_retryable, retry_after, remediation, policy_rejection}`; `From<TransactionManagerError>`, `From<ExecutorError>`, `From<StateStoreError>` for `WalletKitError`.
- Consumes: port errors from `crate::core::deps` (`RpcError`, `SignerError`, `PolicyEngineError`, `GasOracleError`, `NonceManagerError`, `SubmissionError`, `StateStoreError`) and `crate::core::wallet::{TransactionManagerError, ExecutorError, PolicyRejection}`.

- [ ] **Step 1: Write `src/error.rs`**

```rust
//! The public error taxonomy (SPEC §5.5). Per-port `{Trait}Error`s remain the internal
//! contracts; `WalletKitError` is the one umbrella every `Wallet` operation returns,
//! classified for retry with a machine-readable [`ErrorKind`].

use crate::core::deps::{
    GasOracleError, NonceManagerError, PolicyEngineError, RpcError, SignerError,
    StateStoreError, SubmissionError,
};
use crate::core::wallet::{ExecutorError, PolicyRejection, TransactionManagerError};
use alloy_primitives::Address;
use std::time::Duration;

/// Machine-readable retry classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A transient failure — retrying the same request may succeed.
    Retryable,
    /// A permanent failure — retrying will not help.
    Terminal,
    /// The tx may already be in flight or the chain moved — reconcile before acting.
    NeedsReconcile,
}

/// The one error every `Wallet` operation surfaces.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WalletKitError {
    #[error(transparent)]
    Rpc(RpcError),
    #[error(transparent)]
    Signer(SignerError),
    /// Policy denied the intent — carries the exact rule + offending field.
    #[error(transparent)]
    Policy(PolicyRejection),
    /// The policy engine failed operationally (load/eval), distinct from a denial.
    #[error(transparent)]
    PolicyEngine(PolicyEngineError),
    #[error(transparent)]
    Gas(GasOracleError),
    #[error(transparent)]
    Nonce(NonceManagerError),
    #[error(transparent)]
    Submission(SubmissionError),
    #[error(transparent)]
    Store(StateStoreError),
    #[error("simulation rejected: {reason}")]
    Simulation { reason: String },
    #[error("signer {signer} does not control the intent account {intent}")]
    AccountMismatch { intent: Address, signer: Address },
}

impl WalletKitError {
    /// The retry classification a caller should branch on.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Rpc(e) => rpc_kind(e),
            Self::Gas(GasOracleError::Rpc(e)) => rpc_kind(e),
            Self::Gas(GasOracleError::CeilingExceeded { .. }) => ErrorKind::Terminal,
            Self::Nonce(NonceManagerError::Rpc(e)) => rpc_kind(e),
            Self::Nonce(NonceManagerError::Store(e)) => store_kind(e),
            Self::Submission(e) => submission_kind(e),
            Self::Store(e) => store_kind(e),
            Self::Signer(_)
            | Self::Policy(_)
            | Self::PolicyEngine(_)
            | Self::Simulation { .. }
            | Self::AccountMismatch { .. } => ErrorKind::Terminal,
        }
    }

    /// Whether an immediate retry of the same request is worthwhile.
    pub fn is_retryable(&self) -> bool {
        self.kind() == ErrorKind::Retryable
    }

    /// A suggested minimum backoff. `None` in Phase 1 — a real value arrives when the
    /// Transport surfaces server `Retry-After`/rate-limit hints (sub-project E); until
    /// then, [`is_retryable`](Self::is_retryable) is the signal and the host paces retries.
    pub fn retry_after(&self) -> Option<Duration> {
        None
    }

    /// A short operator hint, when one is more useful than the error message alone.
    pub fn remediation(&self) -> Option<&'static str> {
        match self {
            Self::AccountMismatch { .. } => {
                Some("sign with the key that controls the intent account")
            }
            Self::Gas(GasOracleError::CeilingExceeded { .. }) => {
                Some("raise gas_ceiling or wait for the base fee to fall")
            }
            Self::Signer(
                SignerError::ApprovalExpired
                | SignerError::ApprovalMismatch
                | SignerError::FeesExceedApproval,
            ) => Some("re-submit the intent to obtain a fresh policy approval"),
            Self::Simulation { .. } => {
                Some("the transaction would revert — inspect calldata and account state")
            }
            _ => None,
        }
    }

    /// The structured rejection when policy denied the intent.
    pub fn policy_rejection(&self) -> Option<&PolicyRejection> {
        match self {
            Self::Policy(r) => Some(r),
            _ => None,
        }
    }
}

fn rpc_kind(e: &RpcError) -> ErrorKind {
    match e {
        RpcError::Call { transient: true, .. } => ErrorKind::Retryable,
        RpcError::Call { transient: false, .. } => ErrorKind::Terminal,
    }
}

fn submission_kind(e: &SubmissionError) -> ErrorKind {
    if e.is_already_accepted() {
        ErrorKind::NeedsReconcile
    } else if e.is_transient() {
        ErrorKind::Retryable
    } else {
        ErrorKind::Terminal
    }
}

// `StateStoreError` is uninhabited in Phase 1 (the in-memory store never errors), so this
// is never actually reached; a durable backend (sub-project B) will make store I/O
// retryable, which this already reflects.
fn store_kind(_e: &StateStoreError) -> ErrorKind {
    ErrorKind::Retryable
}

impl From<TransactionManagerError> for WalletKitError {
    fn from(e: TransactionManagerError) -> Self {
        use TransactionManagerError as E;
        match e {
            E::AccountMismatch { intent, signer } => Self::AccountMismatch { intent, signer },
            E::SimulationRejected { reason } => Self::Simulation { reason },
            E::Denied(r) => Self::Policy(r),
            E::Rpc(e) => Self::Rpc(e),
            E::Gas(e) => Self::Gas(e),
            E::Policy(e) => Self::PolicyEngine(e),
            E::Nonce(e) => Self::Nonce(e),
            E::Signer(e) => Self::Signer(e),
            E::Store(e) => Self::Store(e),
            E::Submission(e) => Self::Submission(e),
        }
    }
}

impl From<ExecutorError> for WalletKitError {
    fn from(e: ExecutorError) -> Self {
        use ExecutorError as E;
        match e {
            E::Rpc(e) => Self::Rpc(e),
            E::Gas(e) => Self::Gas(e),
            E::Policy(e) => Self::PolicyEngine(e),
            E::Nonce(e) => Self::Nonce(e),
            E::Signer(e) => Self::Signer(e),
            E::Store(e) => Self::Store(e),
            E::Submission(e) => Self::Submission(e),
        }
    }
}

impl From<StateStoreError> for WalletKitError {
    fn from(e: StateStoreError) -> Self {
        Self::Store(e)
    }
}
```

- [ ] **Step 2: Add classification tests to `src/error.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_rpc_is_retryable_with_no_backoff_yet() {
        let e = WalletKitError::Rpc(RpcError::Call {
            message: "timeout".into(),
            transient: true,
        });
        assert_eq!(e.kind(), ErrorKind::Retryable);
        assert!(e.is_retryable());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn non_transient_rpc_is_terminal() {
        let e = WalletKitError::Rpc(RpcError::Call {
            message: "method not found".into(),
            transient: false,
        });
        assert_eq!(e.kind(), ErrorKind::Terminal);
        assert!(!e.is_retryable());
    }

    #[test]
    fn already_known_submission_needs_reconcile() {
        // "already known" is a canonical already-accepted message -> reconcile, don't fail.
        let e = WalletKitError::Submission(SubmissionError::Rpc(RpcError::Call {
            message: "already known".into(),
            transient: false,
        }));
        assert_eq!(e.kind(), ErrorKind::NeedsReconcile);
    }

    #[test]
    fn ceiling_exceeded_is_terminal_with_remediation() {
        let e = WalletKitError::Gas(GasOracleError::CeilingExceeded {
            ceiling: 100,
            needed: 200,
        });
        assert_eq!(e.kind(), ErrorKind::Terminal);
        assert!(e.remediation().is_some());
    }

    #[test]
    fn denial_exposes_the_structured_rejection() {
        let rejection = PolicyRejection {
            rule: "spend_limit".into(),
            field: Some("value".into()),
            reason: "exceeds cap".into(),
        };
        let e = WalletKitError::from(TransactionManagerError::Denied(rejection));
        assert_eq!(e.kind(), ErrorKind::Terminal);
        let r = e.policy_rejection().expect("policy rejection present");
        assert_eq!(r.rule, "spend_limit");
        assert_eq!(r.field.as_deref(), Some("value"));
    }

    #[test]
    fn from_txmgr_flattens_domain_variants() {
        let acct = Address::ZERO;
        let e = WalletKitError::from(TransactionManagerError::AccountMismatch {
            intent: acct,
            signer: acct,
        });
        assert!(matches!(e, WalletKitError::AccountMismatch { .. }));
        let e = WalletKitError::from(TransactionManagerError::SimulationRejected {
            reason: "revert".into(),
        });
        assert!(matches!(e, WalletKitError::Simulation { .. }));
    }
}
```

- [ ] **Step 3: Wire `src/lib.rs`** — replace line 14 and add the module:

```rust
pub mod error;

pub use error::{ErrorKind, WalletKitError};
pub use facade::{Runner, Wallet, WalletBuilder};
```

- [ ] **Step 4: Update `src/facade.rs`** — delete the `WalletError` enum (the `#[derive(Debug, thiserror::Error)] #[non_exhaustive] pub enum WalletError { … }` block near the end) and its doc comment. Add `use crate::error::WalletKitError;`. Replace every `Result<_, WalletError>` return type in `send`/`handle`/`status`/`tick` with `Result<_, WalletKitError>`. The `?` operator continues to work via the new `From` impls. (The facade test does not reference `WalletError`.)

- [ ] **Step 5: Update `tests/localnet.rs`** — in `overspend_rejects_and_recycles_the_nonce`, replace `use walletkit::WalletError;` with `use walletkit::WalletKitError;`, drop the now-unused `use walletkit::core::wallet::TransactionManagerError;`, and change the assertion:

```rust
    assert!(
        matches!(err, WalletKitError::Submission(_)),
        "expected a deterministic submit reject, got {err:?}"
    );
```

- [ ] **Step 6: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets`
Expected: fmt clean; clippy 0 warnings; all unit tests pass (new classification tests included) + 8 localnet. Report real output, leave uncommitted, commit on approval:
`git commit -m "feat(error): unified WalletKitError taxonomy with retry classification"`

---

## Task 2: `tracing` dependency + `obs` shim

**Files:**
- Modify: `Cargo.toml`
- Create: `src/obs.rs`
- Modify: `src/lib.rs` (add `pub(crate) mod obs;`)

**Interfaces:**
- Produces: `crate::obs::{info, warn, error, debug, trace}` macros usable at any call site with no `cfg`; the `tracing` feature (default-on) toggling real vs no-op.

- [ ] **Step 1: Edit `Cargo.toml`** — add under `[dependencies]` (keep the existing alphabetical-ish grouping):

```toml
# Observability: emit-only (the host installs the subscriber / OTLP export). Optional but
# on by default; a `default-features = false` build strips it to no-ops via src/obs.rs.
tracing = { version = "0.1", optional = true, default-features = false, features = ["std", "attributes", "log"] }
```

Add a `[features]` `default` and the `tracing` feature (the section already has `policy-moonpay`):

```toml
[features]
default = ["tracing"]
tracing = ["dep:tracing"]
policy-moonpay = ["dep:wasmtime", "dep:wasmtime-wasi", "dep:time"]
```

Add under `[dev-dependencies]`:

```toml
tracing-subscriber = { version = "0.3", default-features = false, features = ["fmt", "registry"] }
```

- [ ] **Step 2: Write `src/obs.rs`**

```rust
//! Observability shim. Instrumentation call sites use `crate::obs::{info,warn,error,
//! debug,trace}!` and `#[cfg_attr(feature = "tracing", tracing::instrument(...))]` so no
//! `cfg` leaks into logic. With the `tracing` feature on these forward to `tracing`; with
//! it off they compile to no-ops (arguments are still type-checked but produce no code).

#[cfg(feature = "tracing")]
pub(crate) use tracing::{debug, error, info, trace, warn};

#[cfg(not(feature = "tracing"))]
mod noop {
    // Event macros accept `target:`/`level` tokens and `key = value` fields; swallow all.
    macro_rules! debug { ($($t:tt)*) => {{}}; }
    macro_rules! error { ($($t:tt)*) => {{}}; }
    macro_rules! info  { ($($t:tt)*) => {{}}; }
    macro_rules! trace { ($($t:tt)*) => {{}}; }
    macro_rules! warn  { ($($t:tt)*) => {{}}; }
    pub(crate) use {debug, error, info, trace, warn};
}

#[cfg(not(feature = "tracing"))]
pub(crate) use noop::{debug, error, info, trace, warn};
```

> Note: the no-op macros intentionally discard their tokens. Because instrumentation is additive glue with no return value, discarding is safe. Function spans use `#[cfg_attr(feature = "tracing", tracing::instrument(...))]`, which simply doesn't apply the attribute when the feature is off — no shim needed for spans.

- [ ] **Step 3: Wire `src/lib.rs`** — add near the other module declarations:

```rust
pub(crate) mod obs;
```

- [ ] **Step 4: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets`
Also verify the off-path compiles: `cargo clippy --no-default-features`
Expected: all green with and without default features (no instrumentation exists yet, so this only proves the shim + feature wiring compile both ways). Commit on approval:
`git commit -m "build(obs): tracing optional dep behind default-on feature + no-op shim"`

---

## Task 3: Core instrumentation (pipeline + executor + signing)

**Files:**
- Modify: `src/core/wallet/transaction_manager.rs`, `src/core/wallet/executor/mod.rs`, `src/core/wallet/signing.rs`, `src/facade.rs`

**Interfaces:**
- Consumes: `crate::obs` macros; `#[cfg_attr(feature = "tracing", tracing::instrument(...))]`.
- Produces: no API change — spans/events only.

- [ ] **Step 1: Instrument the send root span** — in `transaction_manager.rs`, annotate `send`:

```rust
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            name = "wallet.send",
            level = "info",
            skip_all,
            fields(intent_hash = ?intent.hash(), account = %intent.account, chain_id = intent.chain_id)
        )
    )]
    pub async fn send(&self, intent: &TxIntent) -> Result<TxHandle, TransactionManagerError> {
```

- [ ] **Step 2: Add stage events in `build_sign_submit`** — using `crate::obs`:
  - after a successful `submit` (the `Ok(_) => {}` arm) and before returning the live handle, emit: `crate::obs::info!(tx_hash = ?tx_hash, nonce, "transaction submitted");`
  - in the transient/already-accepted arm: `crate::obs::warn!(nonce, "submission indeterminate; assuming sent");`
  - in the deterministic-reject arm, before `return Err`: `crate::obs::error!(error = %e, nonce, "submission rejected; nonce recycled");`
  - right after `let nonce = self.nonce_manager.allocate(account).await?;` in `send`: `crate::obs::debug!(nonce, "nonce allocated");`

  (`tx_hash`/`nonce` are already in scope; no secret fields.)

- [ ] **Step 3: Redact the signing call** — in `signing.rs`, annotate `sign_encode` (which takes the signer + approval):

```rust
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "sign", level = "debug", skip_all, fields(intent_hash = ?intent_hash))
    )]
```

  Confirm no `fields(...)` names any of `signer`, `tx`, `approval`, or output bytes. If `build_tx` is also instrumented, it must be `skip_all` too.

- [ ] **Step 4: Instrument the executor** — in `executor/mod.rs`:
  - annotate `tick`: `#[cfg_attr(feature = "tracing", tracing::instrument(name = "wallet.tick", level = "debug", skip_all))]`
  - annotate `bump`: `#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all, fields(intent_hash = ?handle.intent_hash, nonce = handle.nonce, bump_count = handle.broadcasts.len())))]`
  - in `confirm`, at the point a transition is applied (right after `handle.status = next;`), emit a transition event. Terminal/replaced transitions are notable; others are debug:

```rust
    let prev = std::mem::replace(&mut handle.status, next);
    if handle.status.is_terminal() {
        crate::obs::info!(intent_hash = ?handle.intent_hash, from = ?prev, to = ?handle.status, "transaction settled");
    } else {
        crate::obs::debug!(intent_hash = ?handle.intent_hash, from = ?prev, to = ?handle.status, "status advanced");
    }
```

  (Replace the existing `handle.status = next;` line with this `mem::replace` form so `prev` is available — `next` is moved in, no clone. Keep the following terminal-approval-removal `if self.state_store.put_handle(&handle).await.is_ok() && handle.status.is_terminal()` block intact.)
  - in `bump`, after a successful bump broadcast (`handle.broadcasts.push(tx_hash);`): `crate::obs::warn!(intent_hash = ?handle.intent_hash, nonce = handle.nonce, "bumped fees (RBF)");`
  - in `bump`, the ceiling arm (`Err(GasOracleError::CeilingExceeded { .. }) => return Ok(())`) and the envelope-stop arm: `crate::obs::warn!(intent_hash = ?handle.intent_hash, "bump halted at gas ceiling / approval envelope");`

- [ ] **Step 5: Thinly instrument the facade** — in `facade.rs`, annotate `tick` with `#[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]`. (The `run` loop stays uninstrumented; it just calls `tick`.)

- [ ] **Step 6: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets && cargo clippy --no-default-features`
Expected: all green both ways (77 unit + 8 localnet unchanged — instrumentation adds no test-visible behavior). Commit on approval:
`git commit -m "feat(obs): intent-hash-correlated tracing across pipeline + executor (skip_all on key paths)"`

---

## Task 4: Adapter instrumentation + secret-`Debug` audit

**Files:**
- Modify: `src/adapters/nonce_store.rs`, `src/adapters/gas_oracle.rs`, `src/adapters/public_mempool.rs`, `src/adapters/signers.rs`

**Interfaces:** no API change — DEBUG mechanics events + a `Debug` safety audit.

- [ ] **Step 1: `nonce_store.rs`** — in `LocalNonceManager::allocate`, after computing `nonce` (before the CAS return), emit `crate::obs::debug!(nonce, "nonce assigned");`; in `reset`, `crate::obs::debug!(chain_next, "nonce reconciled to chain");`. No secret fields.

- [ ] **Step 2: `gas_oracle.rs`** — in `RpcGasOracle::bump`, on the ceiling error path emit `crate::obs::warn!(needed, ceiling, "gas bump would exceed ceiling");` before returning it. (Fields are `u128`, no secrets.)

- [ ] **Step 3: `public_mempool.rs`** — in `submit`, emit `crate::obs::debug!("broadcasting signed tx");` (do **not** log the RLP bytes).

- [ ] **Step 4: Secret-`Debug` audit in `signers.rs`** — verify `LocalSigner` does **not** `#[derive(Debug)]` in a way that prints the wrapped `PrivateKeySigner`/key. If it derives `Debug`, replace with a manual impl that prints only the address:

```rust
impl std::fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSigner").field("address", &self.address()).finish_non_exhaustive()
    }
}
```

  (If `LocalSigner` has no `Debug`, leave it — no leak. Do not add instrumentation that records the key.)

- [ ] **Step 5: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets`
Expected: all green. Commit on approval:
`git commit -m "feat(obs): DEBUG mechanics events in adapters + redacting Debug on LocalSigner"`

---

## Task 5: Redaction security test

**Files:**
- Modify: `src/core/wallet/signing.rs` (add a `#[cfg(all(test, feature = "tracing"))]` test module) — the sign path lives here.

**Interfaces:** consumes `tracing-subscriber` (dev) + the `sign` span from Task 3.

- [ ] **Step 1: Write the capturing test** — a custom `Layer` records every span/event field's `Debug`/`Display` value into a shared buffer; the test signs with a **known** key and asserts the key hex never appears and only allow-listed field names are recorded.

```rust
#[cfg(all(test, feature = "tracing"))]
mod redaction_tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    #[derive(Default, Clone)]
    struct Capture(Arc<Mutex<Vec<String>>>);

    struct FieldSink<'a>(&'a Capture);
    impl tracing::field::Visit for FieldSink<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0 .0.lock().push(format!("{}={:?}", field.name(), value));
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, _: &tracing::span::Id, _: Context<'_, S>) {
            attrs.record(&mut FieldSink(self));
        }
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            event.record(&mut FieldSink(self));
        }
    }

    #[tokio::test]
    async fn signing_never_records_key_material() {
        use crate::adapters::LocalSigner;
        use crate::core::deps::Signer;
        use crate::core::wallet::{GasEnvelope, PolicyApproval, TxIntent};
        use alloy_primitives::{Address, Bytes, TxKind, U256};

        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        // Anvil dev mnemonic, account 0 — its private key is KEY_HEX (asserted absent below).
        let signer = LocalSigner::from_mnemonic(
            "test test test test test test test test test test test junk",
            0,
        )
        .expect("signer");
        let account = signer.address();
        let intent = TxIntent {
            chain_id: 1,
            account,
            to: TxKind::Call(Address::from([0xbb; 20])),
            value: U256::ZERO,
            input: Bytes::new(),
            purpose: None,
        };
        let intent_hash = intent.hash();
        let approval = PolicyApproval::mint(intent_hash, GasEnvelope::DEFAULT, u64::MAX);
        let tx = super::build_tx(&intent, 0, 21_000, crate::testutils::estimation(100, 1));
        // The instrumented path (Task 3 Step 3 put `#[instrument(skip_all, fields(intent_hash))]` here).
        let _ = super::sign_encode(&signer, tx, intent_hash, &approval, 0)
            .await
            .expect("sign");

        const KEY_HEX: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let recorded = capture.0.lock().clone();
        for entry in &recorded {
            assert!(!entry.to_lowercase().contains(KEY_HEX), "key leaked: {entry}");
            let name = entry.split('=').next().unwrap_or("");
            // Only the allow-listed field may appear on the sign path (Task 3 Step 3).
            assert!(matches!(name, "intent_hash"), "unexpected sign field: {entry}");
        }
    }
}
```

> The allow-list (`intent_hash`) must match exactly the `fields(...)` chosen in Task 3 Step 3; if Task 3 adds another safe field there, extend the `matches!`. If `LocalSigner::from_mnemonic`'s exact signature differs, use the same constructor `tests/support/mod.rs` uses. The test is a regression guard: removing `skip_all` (auto-recording `signer`/`tx`/`approval`) or adding a leaky field fails the allow-list assertion.

- [ ] **Step 2: Gate + report + commit on approval**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test --all-targets`
Expected: the redaction test passes (no key in any captured field; only `intent_hash` recorded on the sign path). Commit on approval:
`git commit -m "test(obs): assert signing telemetry never records key material"`

---

## Definition of done (sub-project A)

- `WalletKitError` is the single public error; `kind()/is_retryable()/retry_after()/remediation()/policy_rejection()` classify every failure; `tests/localnet.rs` uses the flattened variant.
- `tracing` spans correlate by `intent_hash` across send + executor; levels follow the convention; every signing path is `skip_all`; the redaction test guards it.
- Builds green **with and without** `--no-default-features`.
- Deferred (unchanged): typed `LifecycleEvent`/`Observer`/`Stream`, metrics facade, OpenTelemetry/OTLP, dashboards.
