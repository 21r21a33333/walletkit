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

> **Revision 2026-08-30 — Phase 2 rewritten for Model 1 (see design §2a, §13).** Phase 1's
> assumption that self-relay could "reuse the send pipeline" via signer-threading is unworkable:
> the per-account `AccountExecutor` can't track a tx sent by the relayer. Research-backed
> resolution: **the relayer is a second operated account** — its own `TransactionManager` +
> `AccountExecutor`, both driven by `Wallet::tick()`, under a **configurable policy (default
> `AllowAll`)**. Phase 2's steps below reflect this; the old "add `relayer: Arc<dyn Signer>`,
> pick by `meta.is_some()`" seam is dropped. Phases 1 and 3 are unchanged in shape.

> **Revision 2026-08-30b — Phase 3 corrected after researching Gelato's live API (design
> Revision 2026-08-30b).** Two fixes to Phase-3 Step 1 below: (a) **Gelato signs its own EIP-712
> request** — a distinct `sol!` struct `{ chainId, target, data, user, userNonce, userDeadline }`
> bound to **Gelato's** `GelatoRelay*ERC2771` domain (name/version/`verifyingContract` from
> `@gelatonetwork/relay-sdk`, pinned at impl + confirmed by the live test) — **not** the OZ
> `ForwardRequest`/`nonces` helpers, which are self-relay-only; the Gelato adapter carries its own
> request type + `userNonce` read. (b) There is **no `adapters/relay/self_relay.rs`** (self-relay
> is facade-orchestrated, Phase 2 slice A); Phase 3 adds only `adapters/relay/{mod,gelato}.rs`,
> which give the `Relay` port + `RelayStatus` their first consumer. **Test/PR gate:** Gelato is
> hosted SaaS (no hermetic harness) → stubbed-transport unit tests **plus an env-gated live test**
> (`GELATO_API_KEY` + testnet); **hold the PR until the live test passes** (approved 2026-08-30).

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
works. Ends in one review. **Model 1:** the relayer is a second operated account (design §2a).

**Reuse:** a **second `TransactionManager` + `AccountExecutor`** (the relayer account) — the
existing send/track/confirm core, instantiated again, unchanged; its `send_with` pipeline for
the **outer** tx (nonce/gas/sign/persist/submit/bump/confirm — "auto-resubmission" for free);
I's `Router`/`SubmissionStrategy` (private-route composition); `GasOracle`; the existing signing
gate + `Signer::sign_typed_data` for the request; the H `FaultRpc` harness + anvil.

**Files:** `core/wallet/transaction_manager.rs` (build-sign spine + read helpers + internal
`meta`-carrying send), `adapters/relay/{mod,self_relay}.rs` (new), `adapters/mod.rs`,
`facade.rs` (second manager/executor + `relayer`/`forwarder`/`relayer_policy` + `send_gasless`),
`testutils.rs`, `tests/gasless.rs` (new), `tests/support/mod.rs`.

> **Implementation status (2026-08-30) — slice A DONE (uncommitted), anvil parity DEFERRED to
> slice B.** Built the self-relay path end-to-end, unit-proven, gate-green (fmt · clippy 0-warn
> ×2 · 133 lib tests · rustdoc `-D warnings`). **Deviations discovered while building, all
> deliberate:**
> - **No `adapters/relay/` for self-relay (option b).** The Phase-1 `RelayError` taxonomy
>   (`Submission`/`Signing`/`Forwarder`/`Rejected`) was shaped for an adapter that *broadcasts*;
>   under Model 1 the outer tx runs through the relayer's **full pipeline**, whose error surface
>   is `TransactionManagerError` (nonce/gas/policy/store/…) — far richer than `RelayError` can
>   hold without lossy mapping. So the **facade orchestrates** self-relay directly over the
>   relayer manager (errors stay `WalletKitError`). The `Relay` port/`SignedRequest`/`RelayStatus`
>   remain the public seam and get their first concrete impl in **Phase 3 (Gelato)**, where the
>   HTTP error surface *is* `RelayError`-shaped.
> - **`build_and_sign_forward_request` lives on the user manager, returns `WalletKitError`** (it
>   fuses the forwarder read + the signing gate). Reuses the existing `sign_typed_data` gate.
> - **`relayer(Arc<dyn Signer>)`** (not `PrivateKeySigner`) — consistent with `builder(signer)`
>   and testable with the mock signer.
> - **`verify()` deferred to Phase 3** (YAGNI — its only consumer is the Phase-3 preflight; a
>   generated-but-unused `sol!` `verify` would trip the zero-warning gate). Only `nonces`/
>   `execute`/`ForwardRequestData` were added now (each has a consumer).
> - **No refill on the relayer executor** — re-relaying a displaced gasless outer tx is out of
>   scope for this cut (YAGNI).
> - **Signature encoding (`SignatureEnvelope::as_bytes()`) is unverified on-chain yet** — the mock
>   tests don't recover it; the **anvil parity test (slice B) must confirm** the `v`-byte form the
>   OZ forwarder's `ECDSA.recover` expects (27/28 vs 0/1), adjusting the encode if needed.
> - **Tests trimmed to earn their place:** dropped a `meta`-stamping (struct-init) and a
>   field-mapping (struct-init) test; kept `forwarder_nonce`-revert (error path),
>   `send_gasless`-not-configured (guard), and the self-relay happy path (orchestration).

- [ ] **Step 1 — build-sign spine + read helpers + `meta`-carrying send (design §5/§7; the
  moved Phase-1 Step 2).** In `transaction_manager.rs`: (a) the `sol!` `nonces`/`verify`/
  `execute`/`ForwardRequestData` call types (first consumer is now); (b) `forwarder_nonce`
  (`nonces(from)`) and `verify(request)` sharing one call+decode helper (DRY), inner `gas` via
  `Rpc::estimate_gas`, deadline = `now + Deadline`; (c) `build_and_sign_forward_request(intent,
  forwarder, chain_id, deadline) -> SignedRequest` — build `ForwardRequest`, `TypedData::from_struct`,
  authorize+sign through the **existing** gate with the **user** signer; (d) an internal
  `meta`-carrying send (`send_with` keeps its public signature and delegates with `meta = None`;
  a private `send_with_meta(intent, opts, meta)` stamps `meta` on the handle at build time). All
  fallible returns are `WalletKitError`; helpers map to `RelayError` at the call site.
- [ ] **Step 2 — `SelfRelay` adapter (design §6).** Holds the **relayer's `TransactionManager`**
  and the `forwarder` address. `relay(signed)`: compose `execute(request_data)` calldata (`sol!`
  `encode`) from `signed.request` + `signed.signature`; build the outer `TxIntent { account =
  relayer, to = forwarder, value = request.value, input }`; send via the relayer manager's
  `meta`-carrying path (stamping `MetaContext { forwarder, signer, nonce }`); return the handle.
  `poll` = default (`Settled`) — the outer hash is already known. No signer threading; the
  relayer manager's signer *is* the relayer.
- [ ] **Step 3 — facade + orchestration (design §2a/§10).** `WalletBuilder::relayer(
  PrivateKeySigner)` + `forwarder(Address)` + `relayer_policy(impl Into<...>)` (default
  `AllowAll`). When relayer+forwarder are set: build a second `TransactionManager` +
  `AccountExecutor` bound to the relayer account (its own policy), wire the `SelfRelay` adapter
  over the relayer manager, and make `Wallet::tick()` drive **both** executors. `Wallet::send_gasless(
  intent, impl Into<GaslessOpts>)`: `build_and_sign_forward_request` on the **user** manager
  (Step 1) → hand `SignedRequest` to `relay.relay()`. No relayer/forwarder configured ⇒ clean
  `WalletKitError` (never a panic), **before** any signing.
- [ ] **Tests (regression-worthy only):** `self-relay-composes-private` (`SelfRelay::via(
  Flashbots..)` → the outer tx records the private channel on the **relayer** handle);
  `send_gasless-without-relayer` errors cleanly pre-sign; **two executors both tick** (a
  configured `Wallet` advances the relayer's pending outer tx); **anvil confirm-parity** — deploy
  an OZ `ERC2771Forwarder` + a trivial 2771 target, self-relay a real `execute`, assert
  `Confirmed` and the target saw the **user** as `_msgSender`; extend with `FaultRpc` for
  reorg → no false `Confirmed`. Also `execute-revert → Failed` (inner revert reverts the outer
  tx; H settles it `Failed`, never `Confirmed`).
- [ ] **Phase close:** gate + rustdoc gate · learning article (relayer-as-second-operated-account,
  the payer/authorizer split, running two per-account executors in one `tick`, gasless⊕private-route
  composition, hermetic forwarder testing on anvil) · report · **standards refactor+review pass** ·
  stop uncommitted.

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
