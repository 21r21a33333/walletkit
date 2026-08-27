# Sub-project I — Private Submission (MEV protection) Implementation Plan

**Goal:** Route the same signed intent through an MEV-protected private relay instead of the public mempool, selectable per-tx via `SubmissionOpts`, without changing how inclusion is verified. Foundation for Phase 2 (J, K).

**Architecture:** Evolve the `SubmissionStrategy` port to take `&SubmissionOpts`. Add a `PrivateMev` adapter (Flashbots-native via `alloy-mev`/`mev-share-rs`; generic Protect RPC over the existing `Transport`) and a `Router` combinator that dispatches per-tx on `opts.route`. Persist `SubmissionOpts` on `TxHandle` so bumps and crash-recovery re-broadcast on the original route. `Escalation` drives the bump loop off the existing `broadcasts.len()` counter. Two new `SubmissionError` variants make relay failures un-mistakable for "sent".

**Tech Stack:** Rust edition 2024; `alloy 2.4.1` (pinned); `alloy-mev` + `mev-share-rs` (new deps, verified by the Phase 1 spike); `async-trait`; `serde`. Tests reuse the H fault-harness pattern (`tests/support/`) + anvil.

**Structure:** Three linear phases, each a self-contained reviewable component. Fewer checkpoints by design — the reuse spike folds into Phase 1 (verdict reported, not a separate stop). Order is a hard dependency chain: 1 (seam) → 2 (private route over the seam) → 3 (proof + docs).

## Global Constraints

- **Behavior-preserving default:** no relay identity configured ⇒ the public-mempool path is behaviorally identical to today. Verify by keeping the localnet suite green (22/22) unchanged.
- **Privacy-safety invariant:** a `Private` tx must never broadcast to the public mempool on a bump or after crash-recovery, except via an explicit, persisted, WARN-logged `Escalation::PublicAfter`.
- **No false broadcast:** `RelayAuth`/`RelayRejected` are never classified as "sent"; the nonce is released and state does not advance.
- **House rules:** comments why-not-what; every test earns its place (no serde/struct-init tests — assert behavior); named return structs over tuples; `WalletKitError` on every public fallible API, classified in `kind()`; key paths `#[instrument(skip_all, …)]` (no secret/RLP in telemetry); define types/fields at first consumer (`#[non_exhaustive]` on public enums); reuse before hand-rolling.
- **Per-phase close:** run the whole gate (`cargo fmt --check` · `cargo clippy --all-targets` zero-warnings · `cargo test`, green **with and without** `--no-default-features`) · report the **real** output + the exact single-test re-run command for each test added · run the CLAUDE.md refactor+review pass over the phase · write the learning article for the phase's concepts · **stop uncommitted; commit only on explicit approval.**

## File Structure

```
src/core/deps/submission.rs        # (edit) types, port +opts param, RelayAuth/RelayRejected
src/adapters/public_mempool.rs     # (edit) submit(&self, rlp, &SubmissionOpts)
src/adapters/private_mev.rs        # (new)  PrivateMev adapter
src/adapters/router.rs             # (new)  Router dispatch combinator
src/adapters/mod.rs                # (edit) module wiring
src/core/wallet/primitives/handle.rs  # (edit) + submission: SubmissionOpts (serde default)
src/core/wallet/executor/mod.rs    # (edit) submit call-sites + escalation branch
src/facade.rs                      # (edit) send_with, with_relay_identity, Router wiring
tests/support/mod.rs               # (edit) RecordingStrategy
tests/private_submission.rs        # (new)  invariant tests
Cargo.toml                         # (edit) alloy-mev, mev-share-rs
```

---

## Phase 1 — Routing seam (types + port + dispatch)

**Component:** the whole per-tx routing seam, internal only. Public-mempool path stays byte-identical; nothing private works yet, but every downstream piece has something to plug into. Ends in one review.

**Reuse:** `alloy-mev` / `mev-share-rs` (spike verdict); `serde` derives; `alloy::Url`.

**Files:** `Cargo.toml`, `src/core/deps/submission.rs`, `src/adapters/public_mempool.rs`, `src/adapters/router.rs` (new), `src/adapters/mod.rs`, all existing `submit(rlp)` call-sites + mocks.

- [ ] **Step 1 — reuse spike (inline decision, no separate stop):** Add `alloy-mev` + `mev-share-rs`; confirm they build against pinned `alloy 2.4.1` (tower/transport versions align) and compile a minimal `FlashbotsSignerLayer` + `eth_sendPrivateTransaction` probe (no send). Record the verdict in design §10.1: **reuse** or **thin in-repo fallback** (hand-rolled `FlashbotsSignerLayer` over our `Transport`). Carry the verdict into Phase 2 Step 1.
- [ ] **Step 2 — types (design §4):** Add `SubmissionOpts`, `SubmissionRoute` (`#[default] Public`), `PrivateRoute` (pub fields, `DiscoveryOpts` idiom), `Relay` (`#[non_exhaustive]`), `Escalation`, `Hints` to `submission.rs`. Add a pure `PrivateRoute::validate() -> Result<(), _>` rejecting Flashbots-only knobs (`block_window`/`fast`/`hints`) on a generic relay (called later at `send_with`).
- [ ] **Step 3 — port param:** Add `opts: &SubmissionOpts` to `SubmissionStrategy::submit`.
- [ ] **Step 4 — `PublicMempool`:** accept `&SubmissionOpts`; a `Private` route reaching it is an internal invariant break (`debug_assert`, treat as public). Keep the existing `debug!` broadcast log.
- [ ] **Step 5 — `Router` (design §6):** new combinator holding `Arc<PublicMempool>` + `Option<Arc<PrivateMev>>`; `submit` matches `opts.route` and delegates; `Public` (or `private == None`) is the identity path.
- [ ] **Step 6 — thread the param:** update every existing `submit(rlp)` call-site and all `tests/`/`testutils.rs` mocks to pass `&SubmissionOpts::default()`.
- [ ] **Tests (regression-worthy only):** `PrivateRoute::validate()` rejects generic-relay+Flashbots-knobs and accepts a Flashbots relay with them; `Router` dispatches `Public`→public and `Private`→private via two recording sub-strategies. (No `Default == Public` test — trivial.)
- [ ] **Phase close:** gate · learning article (submission strategies, MEV/private routing, the `Vendor`-style enum modeling) · report · stop uncommitted.

## Phase 2 — Private route, end to end (`PrivateMev` + persistence + executor + facade)

**Component:** a private tx can be sent, persisted, bumped, escalated, and recovered on its original route. This is the feature. Ends in one review.

**Reuse:** Phase 1 spike verdict for the Flashbots path; the existing `Transport` for generic Protect; the existing `broadcasts.len()` bump counter; the existing nonce-release-on-submit-failure path.

**Files:** `src/adapters/private_mev.rs` (new), `src/adapters/mod.rs`, `src/core/wallet/primitives/handle.rs`, `src/core/wallet/executor/mod.rs`, `src/facade.rs`, `src/core/wallet/transaction_manager.rs`, `Cargo.toml`.

- [ ] **Step 1 — `PrivateMev` adapter (design §5):** Flashbots-native path — `eth_sendPrivateTransaction` with `maxBlockNumber = current + block_window`, `fast`, MEV-Share `hints`, authed by the identity signer (per spike verdict). Generic Protect path — `HashMap<Relay, Arc<dyn Rpc>>` (a `Transport` per relay URL), `submit` = `send_raw`; `MevBlocker`/`Bloxroute` URLs as constants, `Custom(Url)` passthrough.
- [ ] **Step 2 — error taxonomy (design §7):** add `SubmissionError::RelayAuth`/`RelayRejected`; map 401/403/identity → `RelayAuth`, relay-decline → `RelayRejected`, transient network → `Rpc`. `is_already_accepted()` returns `false` for both; add `is_relay_terminal()`. Map into `WalletKitError` + classify in `kind()`. `debug!` records `relay` + route (never the RLP).
- [ ] **Step 3 — persist on `TxHandle` (design §6):** add `#[serde(default)] pub submission: SubmissionOpts` (doc: privacy-safety invariant; absent ⇒ Public). Thread the caller's opts through the send path into the handle before the persist-before-broadcast write.
- [ ] **Step 4 — executor integration (design §6):** both submit call-sites (send-bump ~L350, recover ~L157) → `self.submission.submit(rlp, &handle.submission)`. Add the escalation branch in `bump()`: `PublicAfter { cycles }` and `broadcasts.len() >= cycles` → rewrite `handle.submission.route = Public`, WARN, broadcast public (persist the rewrite); else re-send on the persisted route (`StayPrivate` recomputes a fresh `block_window`). A `RelayAuth`/`RelayRejected` return does **not** advance `broadcasts`/`last_broadcast_at` and releases the nonce.
- [ ] **Step 5 — facade (design §6):** `WalletBuilder::with_relay_identity(signer)` stores a swappable endpoint-auth signer; when set, build `PrivateMev` and wrap the strategy in `Router`, else plain `PublicMempool`. `Wallet::send` delegates to `send_with(&intent, &SubmissionOpts::default())`; `send_with` calls `PrivateRoute::validate()` (clear `WalletKitError` on generic+knob combos), then threads `opts` into the pipeline; `send_with(Private)` with no relay identity → clear `WalletKitError` (never a panic).
- [ ] **Tests (regression-worthy only):** relay-failure classification (403→`RelayAuth`, decline→`RelayRejected`, neither `is_already_accepted`); executor no-leak-on-bump (`StayPrivate` re-sends private across N bumps); escalation-at-threshold (route rewrite persisted); no-false-broadcast (relay error → no `broadcasts` growth, nonce released); facade `send_with(Private)` without identity errors cleanly, with identity persists `route = Private`.
- [ ] **Phase close:** gate · learning article (persisted-route privacy-safety, RBF vs private relays, endpoint-auth identity separation, error classification) · report · stop uncommitted.

## Phase 3 — Proof + polish (integration tests + docs)

**Component:** the invariants proven end-to-end over real persistence + anvil, plus user-facing docs. Ends in one review, then the sub-project PR.

**Reuse:** the H fault-harness (`tests/support/`, `FaultRpc`) for the reorg parity test; anvil.

**Files:** `tests/private_submission.rs` (new), `tests/support/mod.rs`, `README.md`, `CHANGELOG.md`, `SPEC.md`.

- [ ] **Step 1 — harness:** `RecordingStrategy` (records `(channel, opts)` per submit) + an in-memory relay stub in `tests/support/`.
- [ ] **Step 2 — no-leak-on-recovery:** persist a `Private` handle through a real `StateStore`, drop + rebuild the executor (crash), drive recovery, assert the re-broadcast hits the private channel. Exercises the `serde` round-trip across the backend.
- [ ] **Step 3 — confirm parity:** over anvil, a privately-routed tx that lands confirms identically; extend with `FaultRpc` to assert no false `Confirmed` under a reorg on the private route (H's guarantee is route-agnostic).
- [ ] **Step 4 — mutation check (H house rule):** temporarily force `submission` to default on recover; assert no-leak-on-recovery fails with a public re-broadcast; revert. Document the mutation in the test module.
- [ ] **Step 5 — docs:** README "Private submission (MEV protection)" section (`send_with`, `with_relay_identity`, the `Escalation` choice, privacy-safety note); `CHANGELOG.md` `[Unreleased]` `Added` + note the `SubmissionStrategy::submit` signature change (pre-1.0 minor = breaking); mark I done on the SPEC Phase-2 line if a marker exists.
- [ ] **Phase close:** full gate incl. `cargo-hack` feature matrix · learning article (fault-injection testing, mutation testing as test-validity proof, reorg-safety across routes) · report · **phase-close standards refactor+review over the whole slice** · then open the PR (`feat/i-private-submission`, CHANGELOG first) and **merge only on your say-so**.

---

## Deferred (not in this plan — tracked)

Submit-time `Fallback` combinator · `mev_sendBundle`/bundles · route-policy predicate (`DisclosurePolicy`/`RouteAllowlist`; `hints` is its seam) · `submit(…, lease)` + distinct `cancel` port method · policy `SelectorAllowlist`/`Velocity`/`check_after` (→ J) · the J (ERC-2771 gasless) and K (approvals/permits) sub-projects.
