# AccountExecutor refactor — functional core / imperative shell, finality-anchored

Status: **planning** (executor 17a–c landed on the current ad-hoc base; this refactor
rewrites it before Task 18/facade). Research-backed (2026-08-22).

## Why refactor (problems in the current executor)

1. **Decision logic is tangled with I/O.** `classify()` calls `rpc.receipt(..)` inside
   the same function that decides the next status and returns `Option<TxStatus>`. It's
   hard to test (needs mock RPC per case) and hard to read.
2. **Reorg is inferred from a single, unvalidated read.** A stale/lagging node
   returning a low `tx_count` or a momentarily-missing receipt is treated the same as a
   real reorg → a false un-mine. This is a documented failure mode:
   - `eth_getTransactionReceipt`/`eth_getTransactionByHash` are **inconsistent after
     reorgs**; cross-node routing returns one entity from a stale fork
     ([geth #28885](https://github.com/ethereum/go-ethereum/issues/28885),
     [#28992](https://github.com/ethereum/go-ethereum/issues/28992)).
   - A node can return a `latest` that "looks current but is stale."
3. **Finality is a depth count off `latest`,** which is weaker than the `finalized`
   block tag and misled by a lagging `latest`.
4. **Duplication.** The executor's bump re-implements the pipeline's
   build → sign → encode → submit.
5. **Coupling.** The executor borrows `TransactionManagerError` from the pipeline module.
6. **Ad-hoc state split.** In-memory `TrackedTx` vs persisted `TxHandle` and the
   pipeline→executor handoff are informal.

## Target architecture: functional core / imperative shell

The idiomatic Rust consensus for externally-driven FSMs is an **enum + pure
`transition(State, Event) -> State`** ([corrode](https://corrode.dev/blog/enums/),
[hoverbear](https://hoverbear.org/blog/rust-state-machine-pattern/)); typestate is for
compile-time API sequencing, `statig` hierarchy is our Phase-3 note. We pair that with
**functional core / imperative shell**: the transition is pure; all I/O and
read-reliability handling live in a thin shell.

```rust
// ---- functional core (pure, no async, no I/O, exhaustive, zero-mock tests) ----
struct FinalityConfig { mode: Finality, required: u64 }   // per chain
enum Finality { Finalized, Depth }                        // finalized tag vs latest-depth

fn transition(state: &TxState, ev: &ChainEvent, cfg: &FinalityConfig) -> Transition
struct Transition { next: Option<TxState>, effects: Vec<Effect> }

enum Effect { Rebroadcast, Bump, Persist, Emit(TxEvent) }  // shell executes these

// ---- imperative shell (AccountExecutor) ----
// per cycle: read a CONSISTENT ChainView once, derive a per-handle ChainEvent
// (hash-anchored), apply transition, run effects.
struct ChainView { latest: u64, finalized: u64, mined_nonce: u64 }
enum ChainEvent {
    NotConsumed,                                   // mined_nonce <= our nonce
    MinedOurs { block: u64, success: bool, final_: bool },
    Superseded { final_: bool },                   // foreign tx at our nonce
    Indeterminate,                                 // bad/stale/inconsistent read -> NO transition
}
```

`TxState` = today's `TxStatus` variants, moved to live with the FSM.

## RPC-robustness (the crux — "a wrong read must not corrupt the lifecycle")

Add ports (alloy passthroughs): `Rpc::finalized_block()`, `Rpc::block_hash(n)`.

1. **Terminal ⇔ finalized.** `Confirmed`/`Failed`/`Replaced` only when the outcome's
   block `<= finalized` (cryptoeconomically irreversible). `Finality::Depth` fallback
   (`latest - block + 1 >= required`) for chains without the tag.
2. **Block-hash anchoring.** A receipt at block `B` with hash `H` is trusted only if
   `rpc.block_hash(B) == H`. Mismatch ⇒ the read is stale/reorged ⇒ `Indeterminate`.
3. **`Indeterminate` = no transition.** A read gap, hash disagreement, or a regressing
   `ChainView` yields **no** state change — retry next cycle. This is the structural
   guarantee that a bad read never advances/rewinds the lifecycle.
4. **Consistent `ChainView`.** Validate `finalized <= latest` and monotonic `latest`
   vs the last cycle; on regression, **skip the whole cycle** (stale node) rather than
   act on it.
5. **Downgrade only on positive evidence.** Un-mine a `Mined` handle only when its
   block is provably non-canonical (hash mismatch) — never on a missing receipt, never
   below `finalized`.

This replaces today's "block_hash changed → Sent" heuristic and the immediate
`Replacing`/`Replaced` inference with evidence-gated, finality-anchored transitions.

## Other cleanups

- **Dedup:** extract `sign_and_encode(signer, tx, intent_hash, &approval, now) -> (Bytes, TxHash)`
  (a small shared helper), used by both the pipeline's initial send and the executor bump.
- **Own error:** `ExecutorError` (wrapping the ports it uses) instead of borrowing
  `TransactionManagerError`.
- **Formalize the handoff:** document/type the in-memory `TrackedTx` (approval + intent
  + fees — never persisted) vs the persisted `TxHandle` (the WAL); `track()` is the
  pipeline→executor seam.

## Testing shift

- **Core:** a `(TxState × ChainEvent × FinalityConfig) → Transition` table, tested
  directly — **no mocks**. Exhaustive and fast.
- **Shell:** a few integration tests with a mock `Rpc` that exercises hash-anchoring
  (stale receipt ⇒ `Indeterminate` ⇒ no change) and finalized-gating.

## Phasing (each a reviewable commit)

- **R1** — ports: `Rpc::finalized_block` + `block_hash`; `FinalityConfig`.
- **R2** — pure core: `TxState`/`ChainEvent`/`Effect`/`transition`; port Confirm's rules
  in with finalized anchoring + `Indeterminate`. Full table tests.
- **R3** — shell: rewrite `recover`/`confirm`/`send`/`tick` around
  `ChainView → event → transition → effects`, with hash-anchored reads + view validation.
- **R4** — dedup `sign_and_encode`; introduce `ExecutorError`; formalize `TrackedTx`/handoff.
- **R5** — migrate tests to the core-table + shell-integration split; delete the old
  mock-heavy classify tests.

## Intent fulfillment / retry — explicitly a Phase 2/3 layer (NOT the executor)

When a tx is `Replaced` (a foreign tx took our nonce), the intent is usually **not
filled** — but the executor must **not** auto-resubmit at a new nonce by default:
- **Double-execution risk:** the replacing tx may have already fulfilled the same
  intent (a manual retry, or another service on the same key). Re-executing a
  mutating action without an **idempotency key** is a double-spend.
- **It often overrides a deliberate cancel:** the most common cause is the key owner
  sending a 0-value self-send to cancel. Resubmitting fights the user.

So "ensure the intent is filled" is a **higher retry/resubmission layer** above the
executor, gated by: replacement **reason** (never resubmit on `cancelled`), an
**idempotency key**, **opt-in per intent**, and a **fresh policy evaluation**. The
mechanics already fit — `HandleId = hash(intent_hash, nonce)` makes a re-attempt a
distinct handle. Needs the deferred `Replaced { by, reason }` enrichment + a
`ResubmitPolicy`. Phase 2/3.

## Single source of truth — remove the parallel `tracking` map

The executor currently holds `tracking: Mutex<HashMap<HandleId, TrackedTx>>` **beside**
the persisted `TxHandle`s. Two stores for the same pending txs → bugs:
- **Leak:** a handle going terminal (Confirmed/Failed/Replaced) never removes its
  `TrackedTx` — the map grows unbounded.
- **Post-restart bump gap:** after a restart the store has the handles (Recover
  rebroadcasts them) but `tracking` is empty, so `send()` can rebroadcast but **never
  bump** them — a stuck tx stays stuck forever.
- **Handoff footgun:** if the caller forgets `track()` after `send()`, the handle is
  tracked for confirm/recover but invisible to bump.

Fix: the **persisted handle is the single source of truth**. It already stores the full
`signed` tx — decode it (alloy `decode_2718`) to recover the tx template + fees for a
bump; reconstruct the intent (chain_id/to/value/input + the account) for a policy
re-eval. The **approval** is the only non-persistable piece — keep it as a *lossy
in-memory cache* (`HashMap<HandleId, PolicyApproval>`); its absence just means
"re-evaluate on next bump" (which post-restart bumps already do). No required
registration, no leak, every pending handle is automatically bump-eligible.

## Concurrency invariants — make them explicit and enforced

The executor's safety currently *assumes* (undocumented, unenforced):
1. **One executor per account.** Two `AccountExecutor`s for the same account would
   double-bump and race the nonce/handle state. Enforce via a registry/typed ownership
   in the facade (Task 18), and document the invariant on `AccountExecutor`.
2. **Non-overlapping `tick()` per account.** The host must not run two ticks for one
   account concurrently. Document; the facade drives one loop per account.

The nonce manager itself is CAS-safe and `reset()` is forward-only (see the matrix
below), so pipeline `send()` concurrent with executor `confirm()` is safe **given**
those two invariants.

## Concurrency & corner-case matrix (current system audit)

Actors: N concurrent pipeline `send()` (same account); one executor `tick()`
(recover→confirm→send). Shared state: nonce state (CAS), handle store (Mutex),
tracking map (Mutex). Verdicts: ✓ handled · ⚠ gap · 🐛 bug · →R refactor fixes it.

| # | Case | Current behavior | Verdict |
|---|---|---|---|
| 1 | Concurrent `send()` allocate nonces | CAS loop → gapless, unique (tested) | ✓ |
| 2 | `send()` allocates just after `confirm()` `reset(mined)` | `reset` forward-only + CAS; high nonce never clawed back | ✓ |
| 3 | `reset(mined)` with stale-**low** `tx_count` | forward-only → no-op | ✓ |
| 4 | `reset(mined)` with stale-**high** `tx_count` (lagging node) | advances `next` past in-flight → nonce gap | 🐛 →R (ChainView validation) |
| 5 | out-of-band tx consumes our nonce | `confirm` → Replacing→Replaced; `reset` reconciles forward | ✓ (refill = Phase 2/3) |
| 6 | `nonce too low` / `already known` on submit | treated as generic error, not success | ⚠ →R (classify as success) |
| 7 | `release()` races `confirm` `reset()` | CAS serializes | ✓ |
| 8 | bump races the tx mining (E8) | best-effort swallow; no abort-if-advanced | ⚠ →R (re-check mined pre-bump) |
| 9 | replacement bump < 10% | `gas_oracle.bump` enforces ceil(+10%) | ✓ |
| 10 | bump hits gas ceiling | stops, leaves tx | ✓ (NOOP-cancel = Phase 2/3) |
| 11 | bump within / beyond envelope | reuse approval / re-evaluate (tested) | ✓ |
| 12 | **post-restart bump** | tracking map empty → can rebroadcast but **never bump** | 🐛 →R (single source of truth) |
| 13 | mined → confirmed at depth | depth-gate (tested) | ✓ → finalized-tag in R |
| 14 | reorg un-mine (block_hash change) | → Sent (tested) but from a **single unvalidated read** | ⚠ →R (hash-anchor) |
| 15 | **stale receipt from a reorged block** | trusted blindly → false transition | 🐛 →R (hash-anchor + Indeterminate) |
| 16 | replacement two-stage depth-gate | Replacing→Replaced (tested) | ✓ |
| 17 | dropped tx (mempool-evicted, no mine) | perpetual rebroadcast; never surfaces `Dropped` | ⚠ (Dropped = deferred) |
| 18 | off-by-one confirmations / head<block skew | `head.saturating_sub(block)+1` clamps ≥1 | ✓ |
| 19 | restart with in-flight txs | `recover` rebroadcasts persisted `signed` | ✓ (bump gap = #12) |
| 20 | crash between persist & broadcast | InMemory loses all (Phase 1) | Phase 3 (durable WAL) |
| 21 | **two executors for one account** | unenforced → double-bump / state race | 🐛 →R (enforce single-owner) |
| 22 | overlapping `tick()` per account | unenforced | ⚠ →R (document/serialize) |
| 23 | **tracking map leak** (terminal handles never removed) | grows unbounded | 🐛 →R (single source of truth) |

### Gaps folded into the refactor (beyond the FSM/finality rework)

- **Classify `nonce too low` / `already known` as success** (#6) — on both send and
  bump; nonce-too-low means already mined → check receipt, don't error/recycle.
- **Re-check mined nonce immediately before a bump** (#8) — abort the bump if the tx
  mined between selection and submit (avoids a pointless nonce-too-low broadcast).
- **Enforce single-executor-per-account + non-overlapping tick** (#21/#22).
- **Stale-high `tx_count` / stale receipt → `Indeterminate`** (#4/#15) — the ChainView
  validation + hash-anchoring.

Deferred (Phase 2/3): `Dropped` detection (#17, needs mempool/timeout rule), NOOP
gap-fill & cancel (#10), intent refill after replacement (#5).

## Test spec — port battle-tested scenarios (R5)

Full catalog in **`docs/plans/2026-08-22-executor-test-matrix.md`** — deduplicated
scenarios mined from geth `legacypool`, reth `transaction-pool`, Alchemy `rundler`,
`ethers-rs`, viem, ethers.js, web3.py, thirdweb `engine-core`, OZ `openzeppelin-relayer`,
Worldcoin `tx-sitter`, Gelato, Safe. Each maps to a `transition(state, event)` table
test (core) or a shell integration test. R5 ports the applicable ones and marks
have / add / defer.

## Non-goals (deferred to Phase 3, per SPEC)

- `statig` hierarchical states (nested quorum/approval).
- Durable event-sourcing (append-only log + replay).
- Cross-RPC quorum/consensus (eRPC's job) and `safe`-tag usage.

## References

- Finality tags: [Alchemy commitment levels](https://www.alchemy.com/overviews/ethereum-commitment-levels),
  [Tatum finality](https://docs.tatum.io/docs/evm-block-finality-and-confidence).
- Reorg read inconsistency: geth [#28885](https://github.com/ethereum/go-ethereum/issues/28885),
  [#28992](https://github.com/ethereum/go-ethereum/issues/28992);
  [BlackTide RPC monitoring](https://blacktide.xyz/blog/web3-monitoring/rpc-endpoint-monitoring/),
  [TRM reorgs](https://www.trmlabs.com/trm-tech-blog/how-trm-handles-blockchain-reorgs-across-evm-chains).
- FSM style: [corrode enums](https://corrode.dev/blog/enums/),
  [hoverbear FSM](https://hoverbear.org/blog/rust-state-machine-pattern/),
  [statig](https://github.com/mdeloof/statig).
