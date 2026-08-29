# Sub-project J — Gasless meta-transactions (ERC-2771): implementation plan

**Goal:** Let a user act without holding ETH — sign an EIP-712 `ForwardRequest` (free); a
gas-paying relayer submits it through an `ERC2771Forwarder` so the target still sees the
*user*. Same policy gate, honest confirmation, a different inclusion mechanism.

**Architecture:** A new `Relay` port with two families — **`SelfRelay`** (compose `execute()`,
submit the outer tx through the *existing* pipeline, so gasless composes with I's private
routes and inherits H's confirm) and **`Gelato`** (HTTP `sponsoredCallERC2771` /
`callWithSyncFeeERC2771`, task-polled). `GaslessOpts` is type-state (invalid family/knob
combos unrepresentable). `TxHandle.meta` drives the confirm-safety decode of
`ExecutedForwardRequest(signer,nonce,success)` — a mined outer tx with `success=false` is
`Failed`, never `Confirmed`.

**Tech stack:** Rust 2024; `alloy 2.4.1` (pinned) `sol!`/`sol-types`/`dyn-abi`; `reqwest`
(from I); `serde`; `async-trait`. **No new deps.** Tests reuse the H `FaultRpc` harness +
anvil with a deployed OZ `ERC2771Forwarder`.

**Structure:** Three linear phases, each a self-contained reviewable component ending in one
review — the same cadence as I. Hard dependency chain: 1 (build+sign+honest-confirm spine) →
2 (self-relay makes it real, proven on anvil) → 3 (managed relay + docs + PR).

## Global constraints

- **Minimal-LOC / reuse-first (the user's explicit ask):** every step names the library
  primitive it reuses; hand-roll nothing `alloy`/`serde`/the existing ports already provide.
  No EIP-712 hand-encoding (`sol!` `SolStruct` + `TypedData::from_struct`); no new HTTP client
  (reuse I's `reqwest`); no new confirm loop (extend the existing tick).
- **Type-state over runtime checks:** `NonceScheme`/`FeeScheme` live on `Gelato` (their only
  capable family); "self-relay + concurrent" / "syncFee without a fee token" don't compile.
- **Confirmation-safety (the J invariant):** a forwarder `execute` that mines with inner
  `success=false` settles `Failed`. Never a false `Confirmed`. This is H's ethic for meta-tx.
- **Secrets:** the Gelato api key is never `Serialize`d, logged, or put on a handle
  (redacting `Debug`; only the non-secret `MetaContext` persists). Redaction test extended.
- **Behavior-preserving:** no relayer/forwarder configured ⇒ `send`/`send_with` and all
  existing behavior are byte-identical; pre-J handles deserialize with `meta = None`.
- **House rules:** comments why-not-what; every test earns its place (assert behavior, no
  serde/struct-init tests); named-return structs over tuples; `WalletKitError` on every public
  fallible API, classified in `kind()`; key paths `#[instrument(skip_all, …)]` (no secret/sig
  in telemetry); `#[non_exhaustive]` on returned enums; define at first consumer.
- **Per-phase close:** full gate (`cargo fmt --check` · `cargo clippy --all-targets` zero
  warnings · `cargo test`, green **with and without** `--no-default-features`) **and**
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked` (the CI rustdoc gate
  that the local fmt/clippy/test gate misses — see I's post-merge fix) · report the **real**
  output + the exact single-test re-run command per test · CLAUDE.md refactor+review pass over
  the phase · learning article · **stop uncommitted; commit only on explicit approval.**

## File structure

```
src/core/deps/relay.rs                     # (new)  Relay port, GaslessOpts type-state, RelayError
src/core/wallet/primitives/gasless/        # (new dir) grouped relayer/meta-tx primitives:
  forward_request.rs                       #   sol! ForwardRequest(+Data), typed_data, verify/nonce
  meta_context.rs                          #   MetaContext (Step 4)
src/core/wallet/primitives/handle.rs       # (edit) + meta: Option<MetaContext> (serde default)
src/core/wallet/primitives/mod.rs          # (edit) exports
src/core/wallet/executor/mod.rs            # (edit) confirm decode + task-poll branch
src/core/wallet/transaction_manager.rs     # (edit) send_gasless orchestration (reuses send_with)
src/adapters/relay/{mod,self_relay,gelato}.rs  # (new) the two families
src/adapters/mod.rs                        # (edit) module wiring + re-exports
src/facade.rs                              # (edit) relayer(), forwarder(), send_gasless(), wiring
src/error.rs                               # (edit) WalletKitError::Relay + kind()
src/testutils.rs                           # (edit) relay mock
tests/gasless.rs                           # (new)  invariant + anvil parity tests
tests/support/mod.rs                       # (edit) forwarder/relay stubs
```

---

## Phase 1 — Gasless core: request + port + honest confirm

**Component:** the build-sign-confirm spine, internal only. A `ForwardRequest` can be built,
turned into policy-gated typed data, and a `meta`-tagged handle confirms **honestly** — but
nothing broadcasts yet. Everything downstream plugs into this. Ends in one review.

**Reuse:** `alloy` `sol!` (`SolStruct`, ABI/event codecs); `alloy_sol_types::eip712_domain!`;
`alloy_dyn_abi::TypedData::from_struct`; the existing `SigningRequest::TypedData` gate +
`Signer::sign_typed_data`; `Rpc::call`/`estimate_gas`; the existing confirm tick.

**Files:** `core/wallet/primitives/forward_request.rs` (new), `core/deps/relay.rs` (new),
`core/wallet/primitives/{handle,mod}.rs`, `core/wallet/executor/mod.rs`, `error.rs`.

- [x] **Step 1 — `ForwardRequest` primitive (design §5) — DONE 2026-08-29:** `sol!` block with
  `ForwardRequest` (signed typehash); `ForwarderDomain { name, version }` (`Cow<'static,str>`,
  OZ defaults); `fn typed_data(&self, forwarder, chain_id, &ForwarderDomain) -> TypedData` via
  `Eip712Domain::new` + `TypedData::from_struct` (runtime domain, so not the static
  `eip712_domain!` macro). Reuses `typed_data_hash`'s zero-chain guard. **YAGNI refinement:** the
  rest of the forwarder ABI (`ForwardRequestData`/`execute`/`nonces`/`verify`/event) is *not*
  defined yet — generated-but-unused codegen trips the zero-warning gate, so each grows in the
  step that first consumes it. Test: golden EIP-712 digest cross-checked against `cast`.
- [~] **Step 2 — request builder helpers → MOVED to Phase 2 (2026-08-29):** `forwarder_nonce`
  (`nonces`) and `verify` (the gasless `dry_run`) share one call+decode helper (DRY), inner
  `gas` via `Rpc::estimate_gas`. **Why moved:** these `pub(crate)` helpers (and the `sol!`
  `nonces`/`verify` call types) have no consumer until `send_gasless` in Phase 2 — building them
  now would dead-code-warn in the plain lib build (fails the zero-warning gate) and violate
  YAGNI. They land in Phase 2 alongside their consumer, returning `RelayError` (now defined).
- [x] **Step 3 — `Relay` port + `GaslessOpts` type-state (design §4) — DONE 2026-08-29:** `Relay`
  trait (`relay` + defaulted `poll`), `RelayStatus`, `SignedRequest` (`#[non_exhaustive]`).
  `GaslessOpts`/`GaslessRoute`/`SelfRelay`/`Gelato`/`FeeScheme`/`NonceScheme`/`Deadline` with bare-verb
  builders + `From` conversions. `Gelato`/`FeeScheme` get a **redacting `Debug`** (no `Serialize`).
  Full `RelayError` (`#[from]` Rpc/Submission/Signer + `Rejected`/`Forwarder`). Done before
  Step 2 because the helpers depend on `RelayError`. Test: `Gelato` `Debug` redacts the api key.
- [x] **Step 4 — `TxHandle.meta` + confirm-safety decode (design §7 — the crux) — DONE
  2026-08-29:** `#[serde(default)] pub meta: Option<MetaContext>` (`MetaContext { forwarder,
  signer, nonce }` in `primitives/gasless/meta_context.rs`, non-secret, `#[non_exhaustive]`;
  `task` deferred to Phase 3 — YAGNI). The fix lives in the **shell** (`AccountExecutor::anchor`
  → new pure `outcome_of`), *not* the FSM: for a `meta` handle, the `Outcome` is `Executed` only
  when the outer receipt succeeded **and** `MetaContext::inner_succeeded` decodes a matching
  `ExecutedForwardRequest(signer,nonce,success=true)`; otherwise `Reverted` (→ `Failed` via the
  existing, unchanged FSM). Added the `ExecutedForwardRequest` event to the `sol!` block (its
  first consumer). Threaded `meta: None` through the 3 existing `TxHandle` sites. Tests:
  `inner_succeeded_only_on_a_matching_success_event` (decode: true/false/absent/mismatch) +
  `gasless_outer_success_does_not_confirm_a_reverted_inner_call` (shell wiring). Full end-to-end
  over a real forwarder is Phase 2's anvil parity test.
- [x] **Step 5 — error wiring — DONE 2026-08-29:** `WalletKitError::Relay(RelayError)` +
  `From<RelayError>`; `relay_kind()` **delegates** (`Rpc`→`rpc_kind`, `Submission`→
  `submission_kind`; `Signing`/`Rejected`/`Forwarder`→Terminal — no re-derived matching);
  `remediation()` hint on `Forwarder`. Test: `relay_errors_classify_by_cause_and_hint_on_config`.
  Manager/executor plumbing of `RelayError` lands in Phase 2 with the gasless send path.

- [x] **Phase-1 close (2026-08-29):** gate green (fmt · clippy 0-warn ×2 · test 130/124 ·
  rustdoc `-D warnings`). **Standards refactor pass applied:** removed a `Phase 2` roadmap
  breadcrumb from a code comment; DRY'd the three `GaslessOpts` `From` impls to route through
  `From<GaslessRoute>` (one place for the default deadline). Slice reviewed against house rules
  (type-state, redaction, `#[non_exhaustive]`, named struct params, reuse, YAGNI) — clean. Left
  uncommitted for review. **Deferred to Phase 2:** the moved Step-2 read helpers; wiring
  `RelayError` through the manager/executor.
- [ ] **Tests (regression-worthy only):** `forward-request-hash` (EIP-712 hash matches an OZ
  fixture vector); `no-false-confirm-on-inner-revert` (`success=false`→`Failed`);
  `success-confirms` (`true`→`Confirmed`, absent→`Failed`); `gelato-secret-redaction`
  (`{:?}` of `Gelato::sponsored(k)` omits the key). No opts/`From`/default tests (trivial).
- [ ] **Phase close:** gate + rustdoc gate · learning article (ERC-2771 & the `_msgSender`
  swap, EIP-712 `SolStruct`, forwarder-nonce vs account-nonce, meta-tx confirmation safety) ·
  report · stop uncommitted.

## Phase 2 — Self-relay end to end (adapter + orchestration + facade + anvil proof)

**Component:** a real gasless tx via self-relay — built, user-signed, relayer-submitted,
tracked, and **confirmed honestly on anvil**, composing with I's private routes. The feature
works. Ends in one review.

**Reuse:** the existing `send_with` pipeline for the **outer** tx (nonce/gas/sign/persist/
submit/bump/confirm — the "auto-resubmission" for free); I's `Router`/`SubmissionStrategy`
(private-route composition); `GasOracle`; the H `FaultRpc` harness + anvil.

**Files:** `adapters/relay/{mod,self_relay}.rs` (new), `adapters/mod.rs`,
`core/wallet/transaction_manager.rs`, `facade.rs`, `testutils.rs`, `tests/gasless.rs` (new),
`tests/support/mod.rs`.

- [ ] **Step 1 — `SelfRelay` adapter (design §6):** holds the relayer `Signer`,
  `Arc<dyn SubmissionStrategy>` (I's Router), `GasOracle`, `Rpc`. `relay()`: compose
  `execute(request_data)` calldata (`sol!` `encode`), build the outer `TxIntent`
  (`account = relayer`, `to = forwarder`, `value = request.value`), submit via the reused send
  path, attach `meta`, return the handle. `poll` = default (`Settled`).
- [ ] **Step 2 — `send_gasless` orchestration (design §4/§7):** in `transaction_manager` —
  build `ForwardRequest` (nonce/gas/deadline), authorize+sign it via the existing gate
  (`SigningRequest::TypedData` + `Signer::sign_typed_data`, **user** signer), hand `SignedRequest`
  to `relay.relay()`. **Outer-tx signing identity (risk §2, DRY):** add `relayer: Arc<dyn Signer>`
  to the manager/executor and select it exactly when `meta.is_some()` (else the account signer) —
  the *same* discriminator that drives the confirm decode, so no new handle field. Bumps re-sign
  by the same rule.
- [ ] **Step 3 — facade (design §10):** `WalletBuilder::relayer(PrivateKeySigner)` +
  `forwarder(Address)`; when both set, wire the `SelfRelay` adapter. `Wallet::send_gasless(
  intent, impl Into<GaslessOpts>)`. `send_gasless` with no relayer/forwarder → clean
  `WalletKitError` (never a panic), before any signing.
- [ ] **Tests (regression-worthy only):** `self-relay-composes-private` (`SelfRelay::via(
  Flashbots..)` → the outer tx records the private channel); `send_gasless-without-relayer`
  errors cleanly pre-sign; **anvil confirm-parity** — deploy an OZ `ERC2771Forwarder` + a
  trivial 2771 target, self-relay a real `execute`, assert `Confirmed` and the target saw the
  user as `_msgSender`; extend with `FaultRpc` for reorg → no false `Confirmed`.
- [ ] **Phase close:** gate + rustdoc gate · learning article (relayer-vs-user key separation,
  reusing the send pipeline as the relay backend, gasless⊕private-route composition, hermetic
  forwarder testing on anvil) · report · stop uncommitted.

## Phase 3 — Managed Gelato + proof + docs (PR close)

**Component:** the managed HTTP family (both fee models, both nonce modes) with task-polled
tracking, then the sub-project close (mutation check, docs, PR). Ends in one review, then the
PR.

**Reuse:** I's `reqwest::Client`; the HTTP status-triage **extracted** from I's
`classify_flashbots` into a shared `adapters/http.rs` helper (both relays use it); the existing
confirm tick (add the task-poll branch — no new loop); the H mutation-testing house rule.

**Files:** `adapters/relay/gelato.rs` (new), `adapters/http.rs` (new — shared status-triage),
`adapters/submission/private_mev.rs` (refactor `classify_flashbots` onto the shared helper),
`adapters/mod.rs`, `core/wallet/executor/mod.rs` (task-poll branch),
`core/wallet/primitives/handle.rs` (+`MetaContext.task`), `tests/gasless.rs`,
`tests/support/mod.rs`, `README.md`, `CHANGELOG.md`, `SPEC.md`.

- [ ] **Step 1 — add `MetaContext.task` + `Gelato` adapter (design §6):** add the
  `task: Option<TaskId>` field now (its first consumer). `reqwest` POST to the ERC-2771
  endpoint — sponsored (`sponsorApiKey`) vs syncFee (`feeToken`); sequential (`userNonce` from
  the shared forwarder-read helper) vs concurrent (omit nonce, unique `salt`). Parse `taskId` →
  task-pending `TxHandle` (`meta.task = Some`). `poll()` GETs status →
  `Pending`/`Included(hash)`/`Failed`. **DRY:** extract I's inline HTTP status-triage
  (`classify_flashbots`) into a shared adapter helper and map it to `RelayError` at the edge —
  do not copy the status matching. Wire format pinned from live docs (risk §4). No secret in any
  span/log (`skip_all`).
- [ ] **Step 2 — task-poll branch (design §7):** in the confirm tick, a handle with
  `meta.task = Some` and no tx hash → `Relay::poll`; `Included(hash)` records the hash (normal
  chain-confirm + the Phase-1 decode take over), `Failed` settles `Failed`. One branch, not a
  new loop.
- [ ] **Step 3 — facade wiring:** `send_gasless(intent, Gelato::…)` routes to the `Gelato`
  adapter; the api key stays in the adapter (built once), never persisted.
- [ ] **Tests (regression-worthy only):** `gelato-task-lifecycle` (stubbed status:
  `ExecPending`→Pending, `ExecSuccess`→`Included`→confirm, `Cancelled`→`Failed`);
  `nonce-mode` (sequential fills `userNonce`, concurrent varies `salt`); `verify-preflight`
  (invalid request rejected pre-relay); **mutation check** (force `meta` to `None` on confirm →
  `no-false-confirm-on-inner-revert` must fail; revert — proves the test bites).
- [ ] **Step 4 — docs:** README "Gasless (ERC-2771)" section (`send_gasless`, `relayer`/
  `forwarder`, `SelfRelay::via` for gasless+private, `Gelato::sponsored/sync_fee`, the
  confirm-safety note); `CHANGELOG.md` `[Unreleased]` `Added`; mark J on `SPEC.md` if a marker
  exists.
- [ ] **Phase close:** full gate incl. `cargo-hack` feature matrix + rustdoc gate · learning
  article (managed vs self relay, fee-abstraction economics, sequential vs concurrent replay
  protection, mutation testing as test-validity proof) · report · **phase-close standards
  refactor+review over the whole slice** · then open the PR (`feat/j-gasless-metatx`,
  CHANGELOG first) and **merge only on your say-so** (merge commit + `--delete-branch`, sync
  `main`, record status).

---

## Deferred (not in this plan — tracked)

`executeBatch` (atomic vs skip-invalid + `refundReceiver`) · policy predicates
`SelectorAllowlist`/windowed `Velocity`/`check_after` (policy slice) · ERC-4337 UserOps /
OpenGSN staking / raw non-2771 relaying · Gelato 1Balance deposit management · a generic
multi-backend `ExecutionBackend` abstraction (earns its place at a third backend) · the K
sub-project (approvals/permits).
