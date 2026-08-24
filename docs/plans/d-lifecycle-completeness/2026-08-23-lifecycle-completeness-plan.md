# Lifecycle Completeness Implementation Plan

> **For agentic workers:** implement task-by-task, review-gated per `CLAUDE.md`. Steps use checkbox (`- [ ]`) syntax.

**Goal:** cancel a stuck tx, settle the cancelled original as `Dropped`, and optionally self-heal a foreign replacement (intent-refill) — reusing the FSM, signing gate, and RBF machinery.

**Architecture:** `cancel` is user-initiated (`TransactionManager` + `Wallet`, a policy-gated 0-value self-send at the target nonce); refill is executor-initiated (reuses a shared `send_intent`). No new FSM states beyond `Dropped`. See `2026-08-23-lifecycle-completeness-design.md`.

**Tech Stack:** Rust 2024; existing alloy/serde stack; no new deps.

## Global Constraints

- Review-gated: one task, full gate, **real** output reported, left **uncommitted**, commit on approval.
- Gate each task: `cargo fmt --all --check` + `cargo clippy --all-targets` + `cargo clippy --all-targets --no-default-features` + `cargo test --all-targets`. Run `cargo clippy --all-targets --features policy-moonpay` when a task touches the policy port.
- No `unwrap`/`expect`/`panic!` in prod (tests only). Public failures return `WalletKitError` (classified in `kind()`).
- Comments describe the code as it is — **no dev-process/task/phase breadcrumbs**; why-not-what; spec anchors (EIP-2831, §5.1) OK.
- New signing/orchestration paths instrumented via `crate::obs` / `#[instrument]`, `skip_all` on key/payload paths.
- Reuse before hand-rolling; extract a shared helper at the 2nd use (the send pipeline).
- `SigningRequest`/`TxStatus` stay `#[non_exhaustive]`.

---

## File Structure

- `src/core/wallet/primitives/signing_request.rs` — `SigningRequest::Cancel(TxIntent)`.
- `src/core/wallet/primitives/handle.rs` — `TxStatus::Dropped` + `is_terminal`; `TxHandle` `cancelled`/`refilled` flags.
- `src/adapters/policy/native.rs` — `Cancel` default-allow-iff-self-send in `decide`.
- `src/core/wallet/signing.rs` — move `decode_fees` here (shared); add the extracted `send_intent` (D-T4).
- `src/core/wallet/transaction_manager.rs` — `cancel`; `send` delegates to `send_intent` (D-T4).
- `src/core/wallet/executor/mod.rs` — `Dropped` mapping in `confirm`; self-send `bump_approval`; refill.
- `src/facade.rs` — `Wallet::cancel`; refill builder knob.
- `src/error.rs` — reject-terminal variant.

---

## Task 1: Cancel types + policy

**Files:** `signing_request.rs`, `handle.rs`, `adapters/policy/native.rs`.

**Interfaces produced:** `SigningRequest::Cancel(TxIntent)`; `TxStatus::Dropped`; `TxHandle.cancelled`/`.refilled`; native `Cancel` default-allow.

- [ ] **Step 1: `SigningRequest::Cancel`** (`signing_request.rs`):

```rust
#[non_exhaustive]
pub enum SigningRequest {
    Transaction(TxIntent),
    Message(Bytes),
    TypedData(Box<TypedData>),
    /// A cancel: a 0-value self-send at a stuck nonce. Carries the self-send intent so the
    /// gate can verify it is genuinely a self-send before default-allowing it.
    Cancel(TxIntent),
}
// in signing_hash(): Self::Cancel(intent) => Ok(intent.hash()),
```

- [ ] **Step 2: `TxStatus::Dropped`** (`handle.rs`) — add the variant and extend `is_terminal`:

```rust
pub enum TxStatus {
    // …existing…
    /// We cancelled this tx: a self-send at its nonce evicted it. Terminal (distinct from
    /// `Replaced`, a foreign tx taking the nonce).
    Dropped,
}
// is_terminal():
matches!(self, Self::Confirmed { .. } | Self::Failed { .. } | Self::Replaced | Self::Dropped)
```
Add two persisted flags to `TxHandle` (serde default `false`):

```rust
    /// Set when `cancel(id)` targeted this handle, so its nonce being consumed settles it
    /// as `Dropped` rather than `Replaced`.
    #[serde(default)]
    pub cancelled: bool,
    /// Set once THIS handle has spawned its refill — a per-handle double-spawn guard
    /// (crash-idempotent), NOT a chain cap. The refilled child is left `false`, so a
    /// re-displaced child refills again; the chain ends only when an attempt is mined.
    #[serde(default)]
    pub refilled: bool,
```
Update every `TxHandle { … }` literal (testutils `handle`, the pipeline, cancel) to set both `false` (or use `..Default::default()` if a `Default` exists — otherwise set explicitly).

- [ ] **Step 3: native policy `Cancel`** (`native.rs`) — default-allow iff a genuine self-send, in `decide`:

```rust
fn is_self_send(i: &TxIntent) -> bool {
    i.to == TxKind::Call(i.account) && i.value.is_zero() && i.input.is_empty()
}

// in decide(request): after the deny-over-allow fold over rules, before default-deny:
let cancel_ok = matches!(request, SigningRequest::Cancel(i) if is_self_send(i));
if !allowed && !cancel_ok {
    return Decision::Deny(PolicyRejection {
        rule: "default-deny".into(), field: None,
        reason: "no policy granted permission".into(),
    });
}
```
(A `Deny` from a rule still short-circuits first, so a strict rule can veto a cancel. A `Cancel` that isn't a self-send falls through to default-deny.) `wasm`/`moonpay` already `else`-deny non-`Transaction`, so they deny `Cancel` unchanged — no edit needed.

- [ ] **Step 4: Tests** (`native.rs`):

```rust
#[tokio::test]
async fn cancel_allows_only_a_genuine_self_send() {
    let engine = DefaultPolicyEngine::new(vec![], Arc::new(FixedClock)); // no rules
    let acct = Address::from([0x11; 20]);
    let self_send = TxIntent { chain_id: 1, account: acct, to: TxKind::Call(acct),
        value: U256::ZERO, input: Default::default(), purpose: None };
    assert!(matches!(engine.evaluate(&SigningRequest::Cancel(self_send)).await.unwrap(), Decision::Allow(_)));

    let not_self = TxIntent { to: TxKind::Call(Address::from([0x22; 20])), ..self_send_clone };
    assert!(matches!(engine.evaluate(&SigningRequest::Cancel(not_self)).await.unwrap(), Decision::Deny(_)));
}
```
(`TxStatus::Dropped` terminality is covered by the settling test in D-T3, not a standalone enum test.)

- [ ] **Step 5: Gate + report; leave uncommitted.** Commit on approval: `feat(lifecycle): Cancel signing request + Dropped status + self-send policy`

---

## Task 2: `cancel(id)`

**Files:** `signing.rs` (move `decode_fees`), `transaction_manager.rs`, `facade.rs`, `executor/mod.rs` (self-send bump), `error.rs`.

**Interfaces produced:** `Wallet::cancel(id)`, `TransactionManager::cancel(id)`.

**Research adopts (cited):**
- **Pre-broadcast fast path:** if the target was never broadcast (`status == Pending`), `cancel` just **releases/recycles the nonce** and persists the handle terminal — no on-chain self-send (cheaper, no footprint). ([thirdweb Engine](https://portal.thirdweb.com/engine/v2/features/transactions))
- **`replacement transaction underpriced` is retryable:** if `submit` returns underpriced (the target was re-priced between our decode and send), re-read the target's fees, bump higher, and resend — don't abandon the cancel. Extend `SubmissionError`/`is_already_accepted`-style handling with an underpriced predicate. ([ethers #3296](https://github.com/ethers-io/ethers.js/issues/3296))
- Gas `21000`, self-send shape (`to==from`, 0-value, empty data), and the +10%-both-fields RBF floor are cited in the design; `gas_oracle.bump` already enforces the floor.

- [ ] **Step 1: Share `decode_fees`.** Move `fn decode_fees(&Bytes) -> Option<(Eip1559Estimation, u64)>` from `executor/mod.rs` to `signing.rs` as `pub(crate)`; update the executor call site to `signing::decode_fees`.

- [ ] **Step 2: `TransactionManager::cancel`** (`transaction_manager.rs`):

```rust
#[cfg_attr(feature = "tracing", tracing::instrument(name = "cancel", level = "debug", skip_all, fields(id = ?id)))]
pub async fn cancel(&self, id: HandleId) -> Result<TxHandle, TransactionManagerError> {
    let mut target = self.state_store.handle(id).await?
        .ok_or(TransactionManagerError::UnknownHandle)?;
    if target.status.is_terminal() {
        return Err(TransactionManagerError::CancelTerminal);
    }
    let account = target.account;
    let self_send = TxIntent {
        chain_id: target.intent.chain_id, account,
        to: TxKind::Call(account), value: U256::ZERO,
        input: Default::default(), purpose: None,
    };
    // RBF: clear the geth price-bump over the target's current fees.
    let (current, _) = signing::decode_fees(&target.signed).ok_or(TransactionManagerError::CancelTerminal)?;
    let fees = self.gas_oracle.bump(current).await?;
    let approval = match self.policy.evaluate(&SigningRequest::Cancel(self_send.clone())).await? {
        Decision::Allow(a) => a,
        Decision::Deny(r) => return Err(TransactionManagerError::Denied(r)),
    };
    let now = self.clock.now_unix();
    let intent_hash = self_send.hash();
    let tx = signing::build_tx(&self_send, target.nonce, 21_000, fees);
    let (rlp, tx_hash) = signing::sign_encode(&*self.signer, tx, intent_hash, &approval, now).await?;
    self.submission.submit(rlp.clone()).await?;

    let cancel = TxHandle {
        id: HandleId::new(intent_hash, target.nonce), account,
        intent: self_send, intent_hash, nonce: target.nonce,
        status: TxStatus::Sent, envelope: approval.gas_envelope(),
        signed: rlp, broadcasts: vec![tx_hash], last_broadcast_at: now,
        cancelled: false, refilled: false,
    };
    self.state_store.put_handle(&cancel).await?;
    target.cancelled = true;
    self.state_store.put_handle(&target).await?;
    info!(nonce = target.nonce, "cancel submitted");
    Ok(cancel)
}
```
Add `TransactionManagerError::{UnknownHandle, CancelTerminal}` and map both to a Terminal `WalletKitError` variant in `error.rs` (`WalletKitError::Cancel` with a `remediation` hint like "the transaction already settled — nothing to cancel"). `submit`'s `already_accepted` case: treat as success (the cancel may already be in the pool) — mirror `bump`.

- [ ] **Step 3: self-send `bump_approval`** (`executor/mod.rs`) — a stuck cancel handle must RBF via the `Cancel` request, else a `Transaction` self-send default-denies:

```rust
let request = if is_self_send(&handle.intent) {
    SigningRequest::Cancel(handle.intent.clone())
} else {
    SigningRequest::Transaction(handle.intent.clone())
};
match self.policy.evaluate(&request).await? { … }
```
Add `pub(crate) fn is_self_send(i: &TxIntent) -> bool` (share the one from native.rs via a `primitives` helper, or duplicate the 1-liner — prefer a `pub(crate)` in `signing.rs`).

- [ ] **Step 4: `Wallet::cancel`** (`facade.rs`):

```rust
/// Cancel a pending tx: a policy-gated 0-value self-send at its nonce (RBF). Errors if the
/// tx already settled. The original settles as `Dropped` once the cancel mines.
pub async fn cancel(&self, id: HandleId) -> Result<TxHandle, WalletKitError> {
    Ok(self.manager.cancel(id).await?)
}
```

- [ ] **Step 5: Tests** — `cancel` rejects a terminal handle (unit, MockStore seeded with a `Confirmed` handle); a stuck cancel handle bumps via `Cancel` (unit: a `MockPolicy` that denies `Transaction` but a real `DefaultPolicyEngine` that allows the self-send `Cancel` still bumps).

- [ ] **Step 6: Gate (incl. `--features policy-moonpay`) + report.** Commit on approval: `feat(lifecycle): Wallet::cancel via policy-gated self-send RBF`

---

## Task 3: Dropped settling

**Files:** `executor/mod.rs`.

- [ ] **Step 1: Map cancelled → Dropped in `confirm`.** After `transition` yields `next`, before persisting:

```rust
let next = if next == TxStatus::Replaced && handle.cancelled { TxStatus::Dropped } else { next };
```
(Place it so the `Replacing` tentative→terminal path also maps: a cancelled handle goes `Sent → Replacing → Dropped`. `Dropped` is terminal, so the `is_terminal` branches — the `info!` settle log and approval eviction — fire as-is.)

- [ ] **Step 2: Localnet test** (`tests/localnet.rs`, add to the matrix): `cancel_settles_original_as_dropped` — mining off, send a low-fee tx, `cancel(handle.id)`, mine; the cancel handle confirms and `status(original) == Dropped`; a fresh send then reuses the freed nonce and confirms.

- [ ] **Step 3: Gate + report.** Commit on approval: `feat(lifecycle): settle a cancelled tx as Dropped`

---

## Task 4: intent-refill (opt-in)

**Files:** `signing.rs`/`transaction_manager.rs` (extract `send_intent`), `executor/mod.rs`, `facade.rs`.

- [ ] **Step 1: Extract `send_intent`.** Pull the body of `TransactionManager::send` into a crate-internal async fn taking the ports it needs (`rpc, gas_oracle, policy, nonce_manager, signer, submission, state_store, clock, gas_buffer_pct, account, intent`) → `Result<TxHandle, TransactionManagerError>`. `TransactionManager::send` becomes a thin wrapper calling it. This is the DRY seam refill reuses.

- [ ] **Step 2: Executor refill.** Add `with_refill_on_replaced(bool)` (default `false`) + a `refill: bool` field. In `confirm`, when a handle settles to `Replaced` and `self.refill && !handle.cancelled && !handle.refilled`:

```rust
// Re-execute the intent a foreign tx displaced, at a fresh nonce + fresh approval.
// `send_intent` re-runs the full policy path (a refill is a fresh authorization), so a
// policy change since the original is honored. The child is NOT marked refilled, so if it
// is displaced too it refills again — the chain continues until an attempt is mined.
if let Ok(new) = send_intent(/* ports */, handle.intent.clone()).await {
    handle.refilled = true; // guard THIS handle only — no double-spawn on restart
    let _ = self.state_store.put_handle(&handle).await;
    debug!(nonce = new.nonce, "intent refilled after replacement");
}
```
**At-least-once until mined (user-directed): no refill cap.** The chain re-fires on each
*foreign* `Replaced` and ends only when an attempt reaches `Confirmed`, hits an on-chain
`Failed` (mined-but-reverted ≠ `Replaced`), or `send_intent` fails best-effort (RPC error, or
the fresh policy/balance check now denies — the natural circuit-breaker). A refill failure is
logged, never aborts the tick. The persisted `refilled` flag is per-handle double-spawn
protection across restart, not a chain marker.
([Fireblocks externalTxId](https://developers.fireblocks.com/reference/api-idempotency), [Turnkey](https://docs.turnkey.com/concepts/introduction))

  **Crash-ordering (settle in impl):** to stay at-most-one-refill-per-displacement across a
  crash, persist the parent's `refilled = true` (or the parent as terminal `Replaced`) *before*
  trusting the spawn — on restart a terminal/`refilled` parent is skipped, so it can't double-spawn.

- [ ] **Step 3: Builder knob** (`facade.rs`) — `WalletBuilder::refill_on_replaced(bool)`, threaded to `AccountExecutor::with_refill_on_replaced` in `build`.

- [ ] **Step 4: Localnet test** — `intent_refilled_after_foreign_replacement`: `no_auto_mine`, send our tx (nonce 0), `steal_nonce(0)` with a foreign tx, mine; with refill **on**, a new handle appears at nonce 1 and confirms and the original is `Replaced` + `refilled`; a control run with refill **off** leaves no new handle. Add `intent_refills_until_mined`: displace **twice** (steal nonce 0, then steal the refill's nonce), and assert a third attempt lands `Confirmed` — proving the chain re-fires past one displacement. (Matrix: in-memory + redb; skip postgres if the harness account collides.)

- [ ] **Step 5: Gate + report.** Commit on approval: `feat(lifecycle): opt-in intent-refill after foreign replacement`

---

## Self-review

- **Spec coverage:** cancel (D-T2), Dropped settling (D-T3), self-send policy (D-T1), intent-refill opt-in (D-T4), reason-classification explicitly deferred (design) — all mapped.
- **Type consistency:** `SigningRequest::Cancel(TxIntent)`, `TxStatus::Dropped`, `TxHandle.cancelled`/`.refilled`, `is_self_send`, `send_intent` names match across tasks; every `TxHandle` literal updated for the new flags (D-T1 Step 2).
- **No placeholders:** each step has concrete code or a concrete instruction referencing a prior task.
- **Observability** folded into the tasks (cancel `skip_all`), not trailing.
