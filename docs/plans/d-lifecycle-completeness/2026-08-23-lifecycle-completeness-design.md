# Sub-project D — Lifecycle Completeness (design)

**Status:** approved 2026-08-23 · **Branch:** `feat/lifecycle-completeness` · **Phase:** 1 robustness (D of A–F, +G) · **Depends on:** A (errors + observability), B (durable state), C (signing surface), on `main`.

## Goal

Complete the transaction lifecycle so a caller can **cancel** a stuck tx, the executor can optionally **self-heal** an out-of-band replacement (**intent-refill**), and every tracked tx **settles to a terminal state** — reusing the existing FSM, signing gate, and RBF machinery rather than adding a parallel path.

## Scope

**In:**
- **`cancel(id)`** — a policy-gated 0-value self-send at the stuck nonce (RBF over the target), tracked to `Confirmed`; the cancelled original settles as **`Dropped`**. Folds in "gap-fill" (a self-send at a stuck nonce is the same operation).
- **`TxStatus::Dropped`** — the terminal state of an original we cancelled (distinct from `Replaced` = a *foreign* tx took our nonce). Delivers "settle, never hang" as a give-up state you *chose*.
- **`SigningRequest::Cancel`** + native-engine **default-allow-iff-genuine-self-send** (closes the smuggling hole; a strict policy can still veto).
- **intent-refill** (opt-in, default off) — on a *foreign* `Replaced`, re-execute the original intent at a fresh nonce + fresh approval, and **keep re-firing on each subsequent displacement until the intent is mined** (or reverts, or the fresh policy/balance gate denies). Per-handle idempotent (one displacement → one refill).

**Out (deferred) — with the phase map so nothing is silently dropped:**

| Deferred item | Lands in | Why |
| --- | --- | --- |
| Reason-classification (`classify(orig, repl)` / `ReplacementReason`) | the **event-API** sub-project | Not wireable over standard RPC (no portable `eth_getTransactionBySenderAndNonce`); its real consumer is labeling lifecycle *events*, which are deferred. Refill instead gates on our own local cancel marker (see below). |
| Typed `LifecycleEvent` / `Stream<TxEvent>` | its own later sub-project | Polling `status()` works today; a library must not own the runtime. |
| `Repriced` classification of our own bumps | n/a | Our bumps mine as `Mined` (same nonce, our hash) — never surface as `Replaced`, so it's unreachable here. |

## Refinement since the design review

The reviewed design proposed **classify-gated** refill (fetch the foreign replacement, skip refill if it's a cancel). Grounding it revealed there is **no portable RPC** to fetch the tx occupying `(sender, nonce)` — so classification can't be wired over a standard node. The refined, RPC-realistic design:

- The **deliberate-cancel** case is handled *locally and exactly*: our `cancel(id)` marks the original, which settles as `Dropped` (never `Replaced`), and **refill only ever fires on `Replaced`** — so a cancel is never refilled.
- The residual footgun — the key owner cancelling **out-of-band with a raw self-send** (not `cancel(id)`) — is a **single-writer anti-pattern** (the documented posture is that the executor exclusively owns the key). It is documented: *use `cancel(id)`; a raw out-of-band self-send under single-writer will be treated as a foreign replacement and refilled.*

(If the reviewer wants deliberate out-of-band-cancel protection, that requires the event/indexer surface and belongs with the deferred classification — flag it at the spec-review gate.)

## Implemented refinements (post code-review, 2026-08-24)

- **No `refilled` field.** Refill is idempotent by construction: a handle terminalizes to `Replaced` and leaves `pending_handles`, so it is never re-processed. Firing refill is gated on that terminal **persist succeeding** — a failed persist keeps the handle non-terminal so it re-transitions next tick, rather than double-spawning. The child is a fresh handle, so a re-displaced child refills again (at-least-once until mined); no per-handle flag is needed.
- **moonpay allows self-send cancels.** The MoonPay OWS engine evaluates a verified self-send `Cancel` through its tx rules (a stuck-tx safety valve) instead of default-denying it; a non-self-send `Cancel` is still denied. The native engine's default-allow is unchanged.
- **cancel un-poisons on failure.** `cancel(id)` persists `cancelled=true` before broadcast; if the broadcast then fails terminally it reverts the flag, so a later foreign displacement settles `Replaced` (refillable), not a spurious `Dropped`.

## Architecture

Two entry points, both reusing existing machinery; no new FSM states beyond `Dropped`:

- **`cancel` is user-initiated** → lives with `send` on `TransactionManager` (+ a `Wallet` facade method). It builds a self-send at the target's nonce and tracks it as its own handle; the existing confirm loop settles both handles.
- **refill is executor-initiated** → the executor already holds every port (it builds/signs/submits during `bump`). On a foreign `Replaced` it re-runs the shared send routine. To avoid duplicating the send pipeline, the send sequence is extracted into one crate-internal `send_intent` used by both `TransactionManager::send` and refill (DRY).

## Components

### Types (`primitives`)
- `SigningRequest::Cancel(TxIntent)` — the 0-value self-send. `signing_hash()` = `intent.hash()`.
- `TxStatus::Dropped` (terminal; `TxStatus` is `#[non_exhaustive]`) — add to `is_terminal()`.
- `TxHandle` gains one persisted `bool` (serde-compatible, default `false`): `cancelled` (→ settle as `Dropped`, not `Replaced`). Refill needs no flag — see *Implemented refinements*.

### Policy (native engine)
- `Cancel(intent)` **default-allows iff** `intent.to == Call(intent.account) && intent.value.is_zero() && intent.input.is_empty()`; otherwise `Deny` (a non-self-send can't ride the cancel path). deny-over-allow still applies. wasm default-denies `Cancel`; moonpay evaluates a self-send `Cancel` through its tx rules (see *Implemented refinements*).

### `cancel(id)` — `TransactionManager::cancel` + `Wallet::cancel`
- Load the handle; **reject if terminal** (`WalletKitError` terminal variant).
- Build `self_send = TxIntent { chain_id, account, to: Call(account), value: 0, input: empty }`; fees = `gas_oracle.bump(decode_fees(handle.signed))` (≥10% RBF over the target); `gas_limit = 21_000`.
- `evaluate(SigningRequest::Cancel(self_send))` → sign (`signing::sign_encode`) → submit; persist a **new cancel handle** at the same nonce (`Sent`). Set `original.cancelled = true` and persist.
- **Re-bump correctness:** `bump_approval` detects a self-send handle and evaluates `SigningRequest::Cancel` (not `Transaction`), so a stuck cancel can still RBF (a `Transaction` self-send would default-deny).

### Dropped settling (executor `confirm`)
- When `transition` yields `Replaced` for a `cancelled` handle, the shell maps it to `Dropped` (both terminal, depth-gated identically). The cancel handle itself confirms as `Mined → Confirmed`. Result: two handles at one nonce, both terminal — no hang.

### intent-refill (executor, opt-in)
- `AccountExecutor::with_refill_on_replaced(bool)` (wired from a `WalletBuilder` knob), default off.
- In `confirm`, when a handle settles to `Replaced` and `!cancelled && !refilled && refill_enabled`: call the shared `send_intent(handle.intent)` (fresh nonce, fresh policy approval), then set **only this handle's** `refilled = true` and persist. The **child is not marked**, so if it too is displaced it refills again — the chain continues until an attempt reaches `Confirmed`. Natural stops: confirmation, an on-chain `Failed` (mined-but-reverted → not `Replaced`), or a best-effort `send_intent` failure (RPC error, or the fresh policy/balance check now denies). `refilled` is per-handle double-spawn protection across restart, not a cap.

## Data flow (cancel)

```
Wallet::cancel(id)
  └─ TransactionManager::cancel
       ├─ load handle; terminal? -> Err(WalletKitError)
       ├─ self_send @ handle.nonce, fees = bump(current), gas 21_000
       ├─ evaluate(Cancel(self_send)) -> approval        (default-allow iff self-send)
       ├─ sign_encode -> (rlp, hash); submit
       ├─ persist new cancel handle (Sent) @ same nonce
       └─ handle.cancelled = true; persist
  … later ticks: confirm() settles the cancel -> Confirmed,
    and the original (nonce consumed, cancelled) -> Dropped.
```

## Error handling

- `cancel` on a terminal handle → a Terminal `WalletKitError` variant (with `remediation`).
- Cancel policy `Deny` (e.g. a strict veto, or a non-self-send smuggling attempt) → `WalletKitError::Policy`.
- Refill re-uses the send pipeline's existing error handling; a refill failure is best-effort (logged), never aborts the tick.

## Testing (each earns its place)

- **Native policy:** `Cancel` allows a genuine self-send, denies a non-self-send (smuggling guard).
- **cancel rejects terminal** (unit).
- **Dropped settling (localnet):** send a low-fee tx (mining off), `cancel(id)`, mine → the cancel confirms and the original is `Dropped`; a fresh send reuses the freed nonce.
- **cancel re-bump (unit):** a stuck cancel handle bumps via the `Cancel` request (doesn't default-deny).
- **intent-refill (localnet):** a foreign tx takes our nonce → with refill on, the intent re-executes at the next nonce and confirms; with refill off, it stays `Replaced`. **Re-fires past one displacement:** displace twice → a third attempt still lands. Per-handle idempotency: a single displacement spawns exactly one refill (no double-spawn).
- No tests for the serde flags or enum plumbing.

## Files touched

`primitives` (SigningRequest, TxStatus + `is_terminal`, TxHandle flags), `adapters/policy/native.rs` (+ wasm/moonpay default-deny Cancel), `core/wallet/signing.rs` or a new `send.rs` (extracted `send_intent`), `transaction_manager.rs` (cancel; call the shared send), `executor/mod.rs` (Dropped mapping, refill, self-send bump), `facade.rs` (`cancel`, refill knob), `error.rs` (reject-terminal variant).

## Prior art & research findings (cited)

A multi-source research pass (viem, ethers, alloy, go-ethereum, reth, OZ Defender, Gelato,
thirdweb Engine, Fireblocks, Turnkey, Safe; EIP-1559/2831/4337) validated the core design
and produced concrete, cited refinements.

**Validated as-is:**
- **Cancel = 0-value self-send to self at the stuck nonce** is exactly EIP-2831 `tx_cancel` and matches viem/ethers' `cancelled` predicate (`data==0x && from==to && value==0`). ([EIP-2831](https://eips.ethereum.org/EIPS/eip-2831))
- **Same-nonce RBF is the only replacement mechanism** — the node keeps the highest-fee tx per `(sender,nonce)`. ([MetaMask](https://support.metamask.io/transactions-and-gas/transactions/how-to-speed-up-or-cancel-a-pending-transaction/))
- **Single-writer** is the production substitute for foreign-tx classification (OZ Defender relies on it). ([Defender](https://docs.openzeppelin.com/defender/module/relayers))
- **Deferring fine-grained repriced/cancelled/replaced sub-typing** is correct — no portable `(sender,nonce)` RPC exists (only non-standard Otterscan `ots_getTransactionBySenderAndNonce`); viem/ethers **scan mined blocks** for `(from,nonce)`. ([reth OtterscanApi](https://reth.rs/docs/reth_ethereum/rpc/struct.OtterscanApi.html))
- **`Dropped` as a chosen give-up (not a wall-clock heuristic)** and **built above alloy** (alloy's watcher can't detect a drop, hangs until timeout). ([alloy PendingTransactionError](https://docs.rs/alloy-provider/latest/alloy_provider/enum.PendingTransactionError.html))
- **cancel is policy-gated, not policy-exempt** — no mainstream custody system exempts cancel. ([Safe #356](https://github.com/5afe/safe/issues/356), [Turnkey](https://docs.turnkey.com/concepts/policies/language))
- **intent-refill is opt-in / default-off and beyond industry norm** — no surveyed system auto-re-executes an intent at a fresh nonce after a nonce-consuming terminal. Keep it guarded. ([Fireblocks statuses](https://developers.fireblocks.com/reference/statuses))
- Our `gas_oracle.bump` already raises **both** EIP-1559 fields by geth's `PriceBump=10` (+10%) plus base-fee coverage. ([go-ethereum legacypool](https://github.com/ethereum/go-ethereum/blob/master/core/txpool/legacypool/list.go))

**Adopted into the plan (cited):**
- **D-T2** treat the node's `replacement transaction underpriced` as **retryable** (re-read fees, bump higher, resend) — the target can be re-priced concurrently. ([ethers #3296](https://github.com/ethers-io/ethers.js/issues/3296))
- **D-T2** **pre-broadcast fast path**: if the target was never broadcast, cancel just **recycles the nonce** — no on-chain self-send. ([thirdweb Engine](https://portal.thirdweb.com/engine/v2/features/transactions))
- **D-T2** cancel gas is hard-coded **21000** (self-send, empty data) — no re-estimation. ([Yellow Paper G_transaction](https://ethereum.github.io/yellowpaper/paper.pdf))
- **D-T3** the `Dropped`-vs-`Replaced` terminal is **chain-authoritative** via nonce reconciliation over hashes we already broadcast (thirdweb's model) — exactly what `event_for` already does; the `cancelled` flag only *labels* the outcome `Dropped`. ([thirdweb engine-core](https://github.com/thirdweb-dev/engine-core/blob/main/README_EOA.md))
- **D-T4** refill is **at-least-once until mined** (user-directed): it re-fires on every *foreign* `Replaced` until an attempt confirms; the per-handle `refilled` flag only stops a single displacement from spawning two refills (crash-idempotent), it does **not** cap the chain. The natural circuit-breaker is the **full policy path re-run on every refill** (a fresh authorization — a now-insufficient balance or a tightened rule denies and ends the chain), plus best-effort send failures. ([Fireblocks externalTxId](https://developers.fireblocks.com/reference/api-idempotency), [Turnkey](https://docs.turnkey.com/concepts/introduction))

**Deferred (reviewer-confirmed 2026-08-23):**
- **`Invalid` 4th terminal** (reth's permanently-unminable state) — deferred; no concrete producer in D beyond `Replaced`/`Failed`. Add later against [reth events](https://reth.rs/docs/src/reth_transaction_pool/pool/events.rs.html) if a real case appears.
- **Optional auto-cancel-after-`valid_until`** (OZ Defender ~8h → NOOP; Gelato retry budget) — deferred; cancel stays explicit, rebroadcast self-heals, and bump already stops at the ceiling. Add later as an opt-in knob if wanted.

## Locked decisions

1. **Lifecycle mechanics only** — cancel + Dropped + intent-refill; event/Stream API deferred.
2. **cancel is policy-visible, default-allow** (native engine), veto-able; not an engine bypass.
3. **Reason-classification (fine-grained) deferred** with the event API (unwireable over standard RPC); the **binary `Dropped`/`Replaced`** decision is chain-authoritative via nonce reconciliation, and refill gates on the local cancel marker + a documented single-writer contract.
4. **gap-fill folds into cancel**; `Dropped` is a chosen give-up state, not a heuristic timeout.
5. **RBF floor = geth +10% on both fields** (already in `gas_oracle.bump`); higher headroom is tunable, not a protocol minimum.
