# WalletKit Executor — Test-Hardening Catalog

> Produced 2026-08-23 by a multi-agent workflow: (1) research the failure modes the
> executor's guards exist for (OZ/viem replacement attribution, lagging-node head
> regression, corrupt-WAL recovery, CAS nonce contention); (2) walk every branch in
> `lifecycle.rs`, `executor/mod.rs`, `nonce_store.rs`, `transaction_manager.rs` for
> uncovered arms; (3) adversarially filter each candidate — must be falsifiable by a
> plausible regression AND distinct from existing coverage.
>
> Stats: 18 failure modes (17 applicable) · 27 uncovered branch-gaps · 48 generated →
> **29 kept** (19 dropped as already-covered/trivial). 4 pure-FSM · 25 shell · 0 localnet.
> All assertions below are the refined post-review assertions.

## Method note — why localnet is empty

Every reorg/head-regression case was pulled *down* to unit-shell: the unit harness can
serve a lower head / stale receipt / non-canonical hash precisely, whereas the localnet
reorg test toggles auto-mine rather than serving a lower head, so it cannot isolate these
guards. The existing 8 localnet scenarios remain the integration backstop.

---

## Unit (pure FSM) — zero mocks, zero harness work

- [ ] **`zero_required_depth_confirms_immediately_at_the_inclusion_block`** — *medium*
  - Pin the `is_final` Depth arm at the `required == 0` boundary (zero-conf L2).
  - `transition(&Sent, &Mined{block:8,Executed}, &view(8,0), &depth(0)) == Some(Confirmed{block:8})` — terminal at the exact inclusion block; pins the `+1` clamp: `saturating_sub(8)+1 = 1 >= 0`.

- [ ] **`replacing_reclaimed_by_our_own_mined_tx_confirms_at_depth`** — *medium*
  - Prove a `Replacing` state is overwritten by a below-depth `Mined` (the Mined arm does not gate on state — a reorg re-including *our* tx must reclaim it).
  - `Replacing{since_block:5}` + `Mined{8,Executed}` at `view(8,0)`/`depth(2)` ⇒ `Some(Mined{8,HASH})` (tentative — fails the instant anyone restricts the Mined arm to Sent/Mined); at `view(9,0)` ⇒ `Some(Confirmed{8})`.

- [ ] **`remine_into_a_different_block_updates_tentative_mined_block`** — *medium*
  - A reorg re-including our tx at a *different* block must advance block+hash (re-key the depth clock), not no-op. Complements the existing same-block no-op test.
  - `Mined{8,HASH_A}` + `Mined{12,HASH_B,Executed}` at `view(12,0)`/`depth(4)` ⇒ `Some(Mined{12,HASH_B})` — full struct: `Some` (not suppressed by `target != state`), block advanced to 12, hash rebuilt to HASH_B.

---

## Unit (shell with mocks) — `Harness`, assert on persisted store + mock call-logs

- [ ] **`receipt_read_error_yields_unknown_holds_state`** — *high* — *needs `MockRpc.receipt_err`*
  - A transient receipt-RPC error must be a no-op, never misread as "not mined" (which would rewind a mined handle). Seed tentative `Mined{8,h1}@n4`; `tx_count:5, head:20, conf:2` (would confirm if anchored); `receipt()` returns `Err(transient)`. After `confirm()`: byte-for-byte still `Mined{8,h1}` (hit `Err(_)=>Unknown` at mod.rs:215). Cross-check: same view with `receipt:None` instead moves to `Replacing{20}` — proving Err path is observably distinct from None.

- [ ] **`bump_denied_by_fresh_policy_leaves_the_tx`** — *high*
  - Policy revoked between send and bump stops the bump without erroring. Sent@n4, `tx_count:4`, `bump:200/1`, `MockPolicy{allow:false}`, DEFAULT envelope, clock 1000, bump_timeout 0, no valid cached approval. Assert `broadcasts.len()==1`, `status==Sent`, `policy.calls==1` (reached `Deny=>Ok(None)` at mod.rs:330, not the envelope/ceiling short-circuits which leave calls==0).

- [ ] **`bump_denied_when_refreshed_envelope_no_longer_admits_the_bump`** — *high*
  - Policy re-approves but with a *tightened* envelope that rejects the bump ⇒ stop, not broadcast. Sent@n4, `handle.envelope=wide DEFAULT` (per-intent cap at 279 passes), `bump:200/1`, `MockPolicy{allow:true, envelope:{150,150}, valid_until:MAX}`, no cached approval. Assert `broadcasts.len()==1`, `status==Sent`, `policy.calls==1` (false arm of `then_some` at mod.rs:329). `calls==1` distinguishes from `bump_beyond_approved_envelope_stops` (calls==0, stops at 279).

- [ ] **`bump_exactly_at_envelope_ceiling_is_admitted`** — *high*
  - Pin the inclusive `<=` in `GasEnvelope::admits` (policy.rs:20-21) — a bump landing exactly on the cap is not stranded. Sent@n4, `handle.envelope={200,1}`, `bump:200/1`, tracked approval `{200,1}`/`valid_until:MAX`. Assert `broadcasts.len()==2`, `policy.calls==0` (cached reuse, so the site is bump_approval:317 boundary). `<=`→`<` mutation makes len stay 1.

- [ ] **`newest_broadcast_receipt_wins_over_stale_older_hash`** — *medium* — *needs per-hash receipt map*
  - After RBF, the newest broadcast's receipt wins; the loop must `continue` past `Ok(None)`. Sent@n4, `broadcasts=[h_old,h_new]`. **Case A**: both receipts present at different blocks (h_old→6/H6, h_new→8/H8), `block_hash(6)=H6, block_hash(8)=H8` ⇒ `Confirmed{8}` not `{6}` (falsifies `.rev()`→forward). **Case B**: `receipt(h_old)=None, receipt(h_new)=8/H8`, `tx_count>nonce` ⇒ `Confirmed{8}` not `Replaced` (hash-anchored receipt overrides by-nonce conclusion, mod.rs:218).

- [ ] **`bump_then_original_mines_bump_receipt_is_ignored_original_wins`** — *medium* — *needs per-hash receipt map*
  - RBF doesn't guarantee the bump wins — if the *original* mines and the bump is receiptless, honor the older mined one. `broadcasts=[h_orig,h_bump]`, receipt only for h_orig@8 (canonical), None for h_bump, `conf:2, head:10`. One `confirm()` ⇒ `Confirmed{8}` (loop `continue`s past receiptless newest to older mined).

- [ ] **`regressed_head_between_cycles_skips_confirm_and_makes_no_transition`** — *medium* — *needs sequenced head + logging nonce mgr*
  - A lagging/failover node serving an *older* head must short-circuit `read_cycle` before any transition or nonce reset. One executor, two `confirm()`s (guard is stateful in `last_latest`). Cycle 1: head 100 → sets last_latest. Cycle 2: head 90 with a would-confirm receipt. Assert still `Sent`; **and** `nonce_manager.reset` not invoked in cycle 2 (short-circuit at mod.rs:183 before reset at 158); last_latest not overwritten to 90. Distinct from `inconsistent_view_skips_the_cycle` (finalized>latest guard).

- [ ] **`receipt_missing_block_anchor_yields_unknown`** — *medium*
  - A receipt with `block_number`/`block_hash` = None (pending/partial shape) ⇒ Unknown, hold state (the guard, not a canonicality read, blocks it). Inline `TransactionReceipt` with None anchors (+ a one-field-Some variant); `canonical:Some(h1)` set so the guard is the sole cause. After `confirm()`: still `Sent`. Catches a `block_number.unwrap_or(0)` regression reaching `block_hash(0)`.

- [ ] **`decode_fees_leaves_a_non_1559_signed_tx_unbumped`** — *medium*
  - `decode_fees` returns None on a non-1559 envelope so bump never acts on a tx it can't reconstruct. Sent@n4 whose `signed` is a valid legacy/EIP-2930 `encoded_2718` body; `bump:200/1`. After `escalate()`: `broadcasts.len()==1`, signed unchanged, `MockGas` 0 bump calls, `policy.calls==0` (returns Ok at mod.rs:269 via `_=>None` at 347).

- [ ] **`decode_fees_leaves_a_handle_with_undecodable_signed_bytes`** — *medium*
  - Garbled `signed` (corrupt WAL) short-circuits the bump, no crash/abort. `signed=[0xff,0x00,0x01]` (bad 2718 type byte ⇒ decode Err). After `escalate()`: Ok, `broadcasts.len()==1`, `status==Sent`, gas.bump 0×, policy.evaluate 0× (`.ok()?` decode-Err path). Pair with the non-1559 sibling (or one table test) so a `.ok()?`→`.expect()` swap panics and a dropped `_=>None` is caught independently.

- [ ] **`bump_transient_submit_error_aborts_without_advancing_broadcasts`** — *medium*
  - A non-already-accepted submit error returns before the mutation block, recording nothing. Sent@n4, `bump:200/1`, `MockSubmit{Transient}`. Prefer direct `exec.bump(&mut handle, 1000)`: (1) `== Err(Submission(_))` (pins mod.rs:296); (2) `submit.seen.len()==1` (bump actually attempted); (3) `broadcasts.len()==1` and persisted `signed` == original (mutation 299-302 skipped). (1)+(2) distinguish from the sibling stop-tests that never call submit. Note: `escalate()` swallows the per-handle Err, so use direct-bump.

- [ ] **`repeated_bumps_across_ticks_append_broadcasts_at_the_same_nonce`** — *medium*
  - Each stuck tick appends (not overwrites) at a stable nonce/id (OZ/thirdweb stable-id contract). `tx_count:4`, `bump:200/1`, `valid_until:MAX`, DEFAULT envelope; `escalate()` ×3. Assert `broadcasts.len()==4`, all hashes distinct, nonce/id unchanged, `status==Sent`, `policy.calls==0`. To also exercise `last_broadcast_at` gating, advance clock + `bump_timeout>0`; else drop that clause.

- [ ] **`bump_records_broadcast_when_replacement_already_known_persists_new_hash`** — *medium* — *strengthen existing `bump_records_broadcast_when_already_known`*
  - The `AlreadyKnown` arm must advance `handle.signed` to the bumped body so `recover()` rebroadcasts the replacement, not the stale original. Same setup + `Submit::AlreadyKnown`. Assert `store.all()[0].signed == bumped RLP` (or `decode_fees` yields 200/1, not the original). Keep `broadcasts.len()==2`; drop the trivial `last_broadcast_at==1000` assert.

- [ ] **`recover_rebroadcasts_only_pending_and_sent_across_multiple_inflight_handles`** — *medium*
  - `recover()`'s `matches!(status, Pending|Sent)` guard rebroadcasts exactly the live handles. Four handles, distinct signed: `Sent@4, Pending@5, Mined{8,B}@6, Replacing{8}@7`. Assert `submit.seen` set == `{h4.signed, h5.signed}`, not the nonce-6/7 bodies. Falsifies both broadening (Mined/Replacing appear) and dropping (len 4).

- [ ] **`recover_swallows_a_per_handle_submit_failure_and_still_attempts_later_handles`** — *medium*
  - The `let _ =` on the per-handle submit result must not abort recovery. Two Sent handles n4/n5, `MockSubmit{Deterministic}` (errors, pushes to `seen` first). Assert `recover().is_ok()` and `seen` set == `{h4.signed, h5.signed}`. `let _ =`→`?` yields `is_err()` + `seen.len()==1`.

- [ ] **`terminal_handles_are_readable_after_restart_but_not_rebroadcast`** — *medium*
  - Terminal = readable but not re-tracked. `Confirmed{8}@4, Failed@5, Replaced@6, Sent@7`, distinct signed. After `recover()`: `submit.seen.len()==1` and `[0]==Sent@7.signed`. Fails if any variant is dropped from `is_terminal()` or the whitelist loosens.

- [ ] **`reset_retains_high_freed_nonce_and_drops_consumed_freed`** — *medium*
  - Pin `free.retain(|n| n >= chain_next)` (`>=` vs `>`) in `reset()` (nonce_store.rs:157). Real `LocalNonceManager` over `InMemoryStateStore`, `pending_nonce:0`. Seed `next=100, free={74,75,150}`; `reset(chain_next=75)`. Assert `free=={75,150}` (74 dropped, **75 retained** — the at-boundary element distinguishes `>=` from `>`), `next==100` (`max(100,75)`). Then `allocate()` recycles lowest-first: `75,150,100,101`.

- [ ] **`sign_failure_recycles_nonce_leaving_no_gap_next_send_reuses_it`** — *low*
  - Release-on-sign-fail re-enters the freed nonce into the allocable set. Real allocator + shared store, `pending_nonce:5`, `MockSigner{ok:false}`. First `send()` ⇒ `Err(Signer(..))` with call-log allocate→release (no submit); a second `MockSigner{ok:true}` manager over the same store reuses the exact nonce, `next` unchanged. Don't assert `free=={5}` (single alloc takes top-shrink). To cover the middle-gap `insert`+recycle (nonce_store:144, 113-116), allocate 5 and 6 and fail on the lower; else redundant with `release_middle_recycles_lowest_first`.

- [ ] **`recover_store_read_error_aborts_the_cycle`** — *medium* — *needs `MockStore.fail_reads`*
  - With `fail_reads` set + a seeded handle, `recover().await == Err(Store(_))` (the `?` at mod.rs:140 propagates). Don't lean on `submit.seen` empty (trivially so). Pin the submit-swallow contract separately (good read + `Submit::Transient` + Sent handle ⇒ `is_ok()` + seen holds that rlp).

---

## Harness extensions required (`src/testutils.rs`)

| Extension | Needed by | Minimal change |
|---|---|---|
| **Per-hash receipt map on `MockRpc`** | newest-wins, bump-then-original | Add `receipts: HashMap<TxHash, TransactionReceipt>` consulted before the scalar `receipt` field (~10 lines). Tests must build handles with distinct non-zero `TxHash`es. Unblocks both replacement-attribution tests. |
| **`receipt()` can return `Err`** | receipt-read-error | Add `receipt_err: bool`; return `Err(RpcError::Call{transient:true})` at top of `receipt()` (~5 lines). |
| **Sequenced `block_number` + logging `MockNonceManager`** | head-regression | `block_numbers: Mutex<VecDeque<u64>>` consumed FIFO (fallback last value), so one executor sees head 100 then 90 (~15 lines); + a call-logging `MockNonceManager` to assert `reset` not called in cycle 2. |
| **`MockStore` can fail reads** | store-read-error | `fail_reads: bool`; `pending_handles()` returns `Err`, keep `put_handle`/`handle` working (additive). |

`receipt_missing_block_anchor`, both `decode_fees` tests, and the CAS/nonce tests are `harness_feasible: true` (inline fixtures / real allocator) — no harness change.

---

## Recommended implementation order (each group = one review-gated task)

1. **Pure FSM gaps** (no mocks, no harness): the 3 pure tests. Highest confidence-per-line.
2. **RBF policy/envelope guards** (existing mocks): `bump_denied_by_fresh_policy`, `bump_denied_when_refreshed_envelope`, `bump_exactly_at_envelope_ceiling`. All *high*.
3. **Bump decode + submit + persistence** (inline fixtures): both `decode_fees`, `bump_transient_submit_error`, `repeated_bumps_across_ticks`, `bump_records...persists_new_hash`, `receipt_missing_block_anchor`.
4. **Recovery + nonce allocator** (real allocator/existing mocks): 3 recover tests, `reset_retains_high_freed_nonce`, `sign_failure_recycles_nonce`.
5. **Harness: per-hash receipt map** → the 2 RBF-attribution tests.
6. **Harness: fallible receipt + fallible store read** → `receipt_read_error` (*high*), `recover_store_read_error`.
7. **Harness: sequenced head + logging nonce mgr** → `regressed_head_between_cycles`. Last: most involved harness change, medium-value guard.
