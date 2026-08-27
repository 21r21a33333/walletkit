# H — Confirmation-safety fault harness: design

**Sub-project:** H (the highest-value slice of SPEC R5 §8). **Date:** 2026-08-27. **Status:** design, pending review.

## 1. Goal & scope

Prove — end-to-end, over the real transport, on a real chain — that walletkit **never reports a false `Confirmed`** when the RPC serves an inconsistent or reorged read. This is the worst failure a wallet library can have: a false confirm tells the caller "your money moved / your tx settled" when it didn't, and everything downstream (accounting, releasing goods, chaining the next tx) acts on a lie. It is silent and irreversible; a transient error or a stuck tx is benign by comparison.

**In scope:** a test-only fault decorator over the `Rpc` port that injects the reads anvil (an honest node) cannot produce, plus the tests that assert no false confirm and correct recovery.

**Explicitly OUT (deferred R5 surface, noted so it isn't lost):** transient-failure recovery, RPC failover, nonce-reconcile-under-fault, and `retry_after`. These are real R5 items but not the highest-stakes property; they remain for a later sub-project.

**Constraint:** test-only. **Zero production-code changes** (the only manifest change is an `async-trait` dev-dependency so the test crate can implement the `Rpc` trait).

## 2. Why this is not already covered

The defense already exists in production and has **unit** coverage against a hand-fed `MockRpc`:

- **Depth / finalized gate** (`is_final`) — don't confirm until buried `required` deep or at/below `finalized`.
- **Head-regression guard** (`AccountExecutor::chain_view`, `last_latest`) — skip a cycle if `latest` went backwards.
- **Block-hash anchoring** (`AccountExecutor::anchor`) — trust a receipt only if `block_hash(receipt.block_number) == receipt.block_hash`; otherwise `ChainEvent::Unknown` → the FSM makes no change.

The gap: **depth + head-regression alone are insufficient**, and anchoring is the load-bearing guard for the worst case. A reorg that replaces block N *while the head keeps advancing* never regresses `latest` (head guard blind) and still satisfies the depth arithmetic (which is just `latest - N + 1 >= required` and has no idea N was orphaned). geth/reth will serve the receipt from the stale fork. Only anchoring catches it — and anchoring only runs on the adversarial read path, which the honest-node localnet suite never exercises. Existing `localnet.rs` covers a *chain-honest* reorg (`reorg_unmines_without_false_confirm_then_recovers`); it does not cover a *lying node*.

H proves the anchoring guard actually fires **in the wired-up executor over the real transport**, not just in a unit test of the pure function.

## 3. Design — `FaultRpc` decorator

`tests/support/fault.rs`:

```
struct FaultRpc { inner: Arc<dyn Rpc>, faults: Arc<Faults> }
struct Faults {              // flipped by the test between ticks; deterministic, no RNG/time
    corrupt_block_hash: AtomicBool,  // block_hash(n) → a constant bogus hash (forces anchor mismatch)
    block_hash_none:    AtomicBool,  // block_hash(n) → Ok(None) (block unresolvable)
    frozen_head:        AtomicU64,   // block_number() → this value when non-zero (stall/regress the head)
}
```

`impl Rpc for FaultRpc` delegates **every** method to `inner` (the real `Transport` over anvil) except:
- `block_number` — returns `frozen_head` when set, else delegates. Lets a test stall or regress the head.
- `block_hash(n)` — returns a bogus hash / `None` per the flags, else delegates. Lets a test make a real, mined receipt fail anchoring (the stale-fork simulation).

`receipt` is **delegated unchanged** — the node honestly returns the receipt for block N; the lie is that N's canonical hash differs, which is precisely how a stale-fork read looks. This is the most faithful model and keeps the decorator tiny.

The test flips a flag, drives `mine + tick`, asserts the status, then clears the flag and asserts recovery. All faults are boolean/counter state behind atomics — fully deterministic, no wall-clock or RNG.

**Harness hook:** `build_wallet` is refactored to take an `Arc<dyn Rpc>` (today it builds `Transport::url` internally); a new `Localnet::fault_wallet(&Arc<Faults>) -> Arc<Wallet>` builds the wallet over `FaultRpc` wrapping a real `Transport`, sharing the same store. One-line change to an existing test helper.

## 4. Scenarios (`tests/fault_injection.rs`)

Run over the in-memory backend only — the guard is on the RPC read path and is **orthogonal to the store**, so repeating across redb/Postgres would earn nothing (house rule: every test earns its place). Skips cleanly when anvil is absent.

1. **`stale_fork_receipt_never_false_confirms_then_recovers`** — send + mine so the tx is genuinely mined and deep enough to confirm. With `corrupt_block_hash` on, tick repeatedly: status must **never** reach `Confirmed` (anchor → `Unknown`). Clear the flag, tick: it confirms. *Proves anchoring rejects a stale-fork receipt end-to-end.*
2. **`regressing_head_skips_and_never_confirms_early`** — with `frozen_head` set below the real head, tick: no premature depth-based confirm (head-regression skip). Clear, tick: confirms. *Proves the head guard over the real loop.*
3. **`unanchorable_receipt_is_ignored`** — with `block_hash_none` on (node can't resolve block N), tick: `anchor` hits its `None` branch → `Unknown`, no confirm. Clear, tick: confirms. *Proves the second anchoring branch.*

Each test is **meaningful by construction**: with the fault active the only way to reach `Confirmed` is to bypass anchoring/head-regression, so the test would fail if that guard were removed. This is verified during implementation by confirming the fault reaches the guarded path.

## 5. Trust boundaries (documented, not tested — irreducible with a single RPC)

- **Fully Byzantine node** that lies *consistently* on both `receipt` and `block_hash(n)` so they agree: anchoring passes. No single-endpoint client can defeat this; it needs multi-endpoint Byzantine cross-checking (out of scope; alloy failover is availability, not agreement).
- **Lying `finalized` tag** (Finalized mode): `is_final` trusts the node's `finalized`; finalization cannot be independently verified from one RPC. Depth mode is the defense for callers who don't trust the tag.
- **Lying-*high* head**: erodes the depth margin (confirms shallower than configured) but the tx must still anchor, so it is a real canonical tx, not a false confirm.

These are recorded here and in the harness module docs so the guarantee's edges are explicit rather than implied.

## 6. Footprint

- `Cargo.toml` — `async-trait` added to `[dev-dependencies]` (test-only).
- `tests/support/fault.rs` — new (`FaultRpc` + `Faults`).
- `tests/support/mod.rs` — expose `fault`; refactor `build_wallet` to take the rpc; add `fault_wallet`.
- `tests/fault_injection.rs` — new (3 scenarios).
- No `src/` change → no `CHANGELOG` entry required (the CI changelog gate keys on `src/`); this is a test deliverable.

## 7. Non-goals

Not re-testing chain-honest paths already in `localnet.rs` (reorg-unmine, stuck-tx, nonce-steal); no production `FaultTransport`; no multi-endpoint failover; no `retry_after`.
