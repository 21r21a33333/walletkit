# Executor test matrix — ported from battle-tested tx managers

Deduplicated correctness scenarios mined (2026-08-22 research) from: go-ethereum
`core/txpool/legacypool`, reth `transaction-pool`, Alchemy `rundler`, `ethers-rs`,
viem, ethers.js, web3.py, thirdweb `engine-core`, OpenZeppelin `openzeppelin-relayer`,
Worldcoin `tx-sitter`, Gelato, Safe.

Status: **[have]** already covered · **[add]** port in R5 · **[defer]** Phase 2/3.
Most become `transition(state, event) -> (state, effects)` table tests (pure core);
a few need a shell integration test (marked *shell*).

## A. Nonce management & concurrency

- [have] **A1 concurrent allocation is gapless & unique** — N tasks allocate → distinct sequential nonces. (ethers-rs NonceManager; our `concurrent_allocations_never_duplicate`.)
- [have] **A2 recycle freed nonce before fresh** — release mid-run → next allocate reuses it. (engine-core recycled_nonces; our `release_middle_recycles_lowest_first`.)
- [have] **A3 release top shrinks high-water & absorbs contiguous** — our `release_top_shrinks…`.
- [have] **A4 reset moves forward only** — stale/low reset never rewinds. (our `reset_moves_forward_only`.)
- [add] **A5 nonce-too-low on submit = already mined → success, not failure** — classify, check receipt, don't recycle. (engine-core E4; OZ NonceTooLow; ethers-rs resync-and-retry-once.) *shell*
- [add] **A6 already-known on (re)broadcast = success** — dedupe, no error, no state regression. (geth `TestDeduplication`; rundler `test_already_known`; OZ AlreadyKnown.) *shell*
- [add] **A7 stale-high `tx_count` must not advance `next` past in-flight** — validated ChainView / Indeterminate; no nonce gap. (our matrix #4; engine-core reset-protection.)
- [add] **A8 nonce regression (reorg) triggers reset-protection, not re-confirm** — cached count 10, RPC returns 7 → don't re-attempt already-confirmed higher nonces. (engine-core `reset_nonces`.)
- [defer] **A9 crash between nonce-persist and broadcast → no permanent gap** — durable WAL rebroadcast/NOOP-fill. (tx-sitter B2; engine-core recovery.) Phase 3.

## B. Replacement / gas bump (RBF)

- [have] **B1 bump ≥ ceil(+10%) on both fee fields** — geth PriceBump on tip AND cap. (geth `TestReplacementDynamicFee`; our gas_oracle `bump_meets_geth_threshold…`.)
- [have] **B2 low-wei strict increase** — ceil + max(old+1). (our `bump_low_wei_still_strictly_increases`.)
- [have] **B3 bump stops at ceiling** — our `bump_errors_at_ceiling…` / `bump_stops_at_gas_ceiling`.
- [have] **B4 bump within envelope reuses approval; beyond re-evaluates** — our `bump_*_envelope_*`.
- [have] **B5 bump keeps the SAME nonce** — RBF, not a new slot. (OZ/tx-sitter A2; implicit in our bump.)
- [add] **B6 bump aborts if the tx mined mid-bump** — nonce advanced between select and submit → no-op, no nonce-too-low broadcast. (engine-core E8 race.)
- [add] **B7 escalation fee grows monotonically per round** — round K > K-1 (compounding). (tx-sitter escalation; our compounding via `bump(prev)` — needs an explicit multi-round test.)
- [defer] **B8 escalation resumes from persisted count after restart** — not reset to 0. (tx-sitter.) Phase 3.
- [add] **B9 tx type immutable across bump** — legacy stays legacy, 1559 stays 1559. (OZ A9; we only build 1559 — assert.)

## C. Confirmation depth / finality

- [have] **C1 mined ≠ confirmed until depth N** — our `confirm_advances…at_required_depth`.
- [have] **C2 reverted receipt → Failed (nonce consumed), only at depth** — our `reverted_receipt_fails_only_at_depth`.
- [add] **C3 finality via `finalized` tag, not just latest-depth** — terminal ⇔ block ≤ finalized. (Alchemy commitment levels.) Core of R1/R2.
- [add] **C4 confirmations clamp ≥1; head<block skew never underflows** — (ethers v5 clamp; our `saturating_sub` — assert explicitly.)
- [add] **C5 skipped/batched blocks still satisfy N confirmations** — head jumps B→B+5 crosses threshold. (viem "repriced (skipped blocks)".)

## D. Reorg / un-mine

- [have] **D1 reorg un-mine → back to Sent** — our `reorg_unmine_returns_handle_to_sent` (naive; R hardens with hash-anchor).
- [have] **D2 replacement reorg frees nonce → Sent** — our `replacement_reorg_frees_the_nonce…`.
- [add] **D3 stale receipt from a reorged block → Indeterminate (no transition)** — hash at that height differs → don't trust it. (geth #28885/#28992; the crux fix.) *shell*
- [add] **D4 reorg deeper than tracked history → requeue everything known** — no silent loss. (rundler `test_reorg_longer_than_history`.)
- [add] **D5 multi-block / sideways / backwards reorg un-mines all replaced txs** — (rundler `test_{forward,sideways,backwards}_reorg`.)
- [add] **D6 reorg changes receipt outcome (success↔revert)** — recompute from canonical, don't strand stale `Confirmed`. (Safe E4.)
- [add] **D7 replaced-then-reorged: replacement un-mined, original resurfaces** — don't get stuck terminal `Replaced`. (viem/ethers gap D4.)
- [defer] **D8 deep reorg past finality = alert, not silent rollback** — a `Confirmed` should never un-confirm. (OZ E5.) Phase 3.

## E. Dropped / replaced classification

- [add] **E1 dropped (evicted, nonce not advanced) → keep polling, NOT replaced** — don't declare replaced prematurely. (ethers `nonce <= replaceable.nonce`.)
- [add] **E2 nonce advanced past us, no matching hash → Dropped/Replaced (settle, don't hang)** — must reach a terminal state. (viem #3875.) *shell*
- [add] **E3 replacement reason classification** — pure `classify(orig, repl)`: repriced (same to/value/data) / cancelled (to==from, value 0, data 0x) / replaced (else). (EIP-2831; viem+ethers identical rule.)
- [add] **E4 classification normalizes address case & nonce encoding** — lowercase `from`, hex nonce still match. (ethers #2133.)
- [defer] **E5 must persist {from,nonce,startBlock,to,value,data} for detection** — hash-only can't classify. (ethers #4875/#3699.) Our handle stores `signed` (has all) → decode; add when reason-enrichment lands.

## F. Restart / recovery

- [have] **F1 restart rebroadcasts persisted in-flight** — our `recover_rebroadcasts_persisted_inflight…`.
- [add] **F2 restart after tx mined during downtime → reconcile to Confirmed, no rebroadcast** — (OZ F2.)
- [add] **F3 boot re-syncs nonce from chain (max of persisted vs on-chain)** — no gap, no reuse. (OZ F3.)
- [add] **F4 post-restart bump works** — the tracking-map gap; single-source-of-truth fix. (our matrix #12.)
- [defer] **F5 idempotent recovery — no double-broadcast across concurrent recovery** — (OZ F6.) Phase 3.

## G. Out-of-band / NOOP

- [have] **G1 foreign tx consumes our nonce → Replaced** — our `replacement_*` (nonce progression).
- [add] **G2 on-chain nonce ahead at send → reconcile against pending, no instant-too-low** — (OZ G2.)
- [defer] **G3 NOOP fills a nonce gap to unstick the queue** — (OZ NOOP semantics.) Phase 2/3.
- [defer] **G4 cancel = higher-fee NOOP at same nonce; cancel of Confirmed rejected** — (OZ `test_cancel_transaction`.) Phase 2/3.
- [defer] **G5 intent refill after Replaced (idempotent, reason-gated, opt-in)** — (see refactor plan.) Phase 2/3.

## H. Gas / validation (mostly at estimate/oracle layer)

- [have] **H1 revert at estimate_gas → SimulationRejected (pre-sign gate)** — our pipeline.
- [add] **H2 tip > fee-cap rejected** — (geth `TestTipAboveFeeCap`; assert our bump never produces tip>cap — coverage keeps cap ≥ tip).
- [defer] **H3 pool-level eviction/underpricing/limits** — we're a sender, not a mempool; N/A unless we add a local queue.

## Invariants (cross-cutting, assert directly)

- [add] **I1 exactly-once execution** — for one intent+nonce, at most one hash ever mines.
- [add] **I2 a Confirmed tx never regresses** (except deep-reorg alert). 
- [add] **I3 every wait/track reaches a terminal or trackable state — never hangs** (viem #3875/#3515).
- [add] **I4 single executor per account; non-overlapping tick** — enforced by the facade.
