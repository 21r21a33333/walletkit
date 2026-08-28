# I — Private submission (MEV protection): design

**Sub-project:** I (the first slice of Phase 2 — the submission-route core + private/MEV-protected broadcast). **Date:** 2026-08-28. **Status:** implemented (Phases 1–2), with the refinements below.

> **Implementation refinements (idiomatic pass).** The final code improves on §4/§7 of this spec in three ways: (1) **type-state routes** — `PrivateRoute::Flashbots(Flashbots) | Protect(Protect)` with the `block_window`/`fast`/`hints` knobs living *only* on `Flashbots`, so a generic relay structurally can't carry them (the "make the unsafe path unrepresentable" rule). This deletes the runtime `PrivateRoute::validate()` and the `GenericRelayOptions` error. (2) **No capability flag** — the "is a relay configured" check is a `SubmissionStrategy::supports_route` method the `Router` implements (private arm is `Option`), checked up front by the pipeline; there is no `private_routing` bool on `Wallet`. (3) **Ergonomic constructors** — `Flashbots::new(esc).fast().within(n)`, `Protect::mev_blocker(esc)`, `impl Into<SubmissionOpts>` on `send_with`, and `relay_identity` (not `with_relay_identity`, matching the builder's bare-verb convention). The relay-error variants carry a `message` (with relay name) rather than a `Relay` field.

## 1. Goal & scope

Route the *same signed intent* through an MEV-protected private channel instead of the public mempool, selectable per-tx by config — same intent, same policy, same hash, different route. This is the foundation the rest of Phase 2 (J gasless meta-tx, K approvals/permits) builds on: it evolves the `SubmissionStrategy` port and adds `SubmissionOpts`, which J and K both need.

**In scope:**
- Evolve the `SubmissionStrategy` port to carry per-send `SubmissionOpts`.
- `SubmissionOpts` / `SubmissionRoute` / `PrivateRoute` / `Relay` / `Escalation` / `Hints` types (repo vocabulary).
- `PrivateMev` adapter: Flashbots-native (`eth_sendPrivateTransaction` with `maxBlockNumber`/`fast`/MEV-Share `hints`) + generic Protect RPC (MEV Blocker / bloXroute / custom).
- `Router` dispatch so one executor-held strategy honors per-tx route.
- Persist `SubmissionOpts` on `TxHandle` so bumps and crash-recovery re-broadcast on the original route (privacy-safety invariant).
- `Escalation` (`StayPrivate` | `PublicAfter { cycles }`) driven by the existing bump counter.
- Rotatable endpoint-auth identity (the `X-Flashbots-Signature` signer, distinct from the tx key).
- `SubmissionError::RelayAuth` / `RelayRejected`, never misclassified as "sent."
- Facade `send_with(intent, opts)`; `WalletBuilder::with_relay_identity(signer)`.

**Explicitly OUT (deferred, noted so it isn't lost):**
- Submit-time `Fallback` combinator (relay-A-down → relay-B). `Escalation::PublicAfter` already covers the important liveness case (non-inclusion over time); endpoint-down-at-submit redundancy is a later slice.
- `mev_sendBundle` / atomic bundles (overlaps Phase-5 batching).
- Route-policy predicate (`DisclosurePolicy` / `RouteAllowlist`) — no consumer yet; the `hints` disclosure seam is documented as its future attachment point.
- The SPEC's `submit(…, lease)` param (Phase-3 distributed-nonce) and a distinct `cancel` port method (earns its place with J's relayer, which has an out-of-band cancel API).
- Policy additions `SelectorAllowlist` / windowed `Velocity` / `check_after` — they gate *what can go gasless*, so they land with J.

**Constraint:** minimal, correctness-first production change, matching Phase-1 house rules. The public-mempool path stays byte-for-byte behaviorally identical when no relay is configured.

## 2. Why the route is a first-class pipeline concern (not transport)

`alloy-mev` is a transport-layer extension, so private routing *could* live in the transport. Rejected: that couples routing policy to connection plumbing and hides the route choice from policy and observability. The route is a first-class pipeline decision — it belongs at the `SubmissionStrategy` seam, where it is visible, persisted, and traceable.

## 3. Vocabulary alignment

The repo has a consistent naming idiom; `Preferences`/`Prefs` is nowhere in it. `Config` = build-time infra wiring (`TransportConfig`, `FinalityConfig`); `Opts` = per-operation caller options (`DiscoveryOpts`). Route selection is the latter.

| Concept | Name | Precedent |
| --- | --- | --- |
| per-send options | `SubmissionOpts` | `DiscoveryOpts` (flat struct + `impl Default`) |
| broadcast channel | `SubmissionRoute` | `SigningRequest`, `TxKind` (domain-prefixed enum) |
| private-route knobs | `PrivateRoute` | `SigningRequest::Transaction(TxIntent)` (tuple payload) |
| private relay endpoint | `Relay` | `Vendor` (`#[non_exhaustive]` managed-endpoint enum) |
| stuck-tx behavior | `Escalation` | `Finality` (plain mode enum) |
| operation variant | `send_with` taking `&SubmissionOpts` | operations take an `&…Opts` param; `with_*` is reserved for builders |

## 4. Port + types

The port gains **one param — `opts`** — nothing else:

```rust
#[async_trait]
pub trait SubmissionStrategy: Send + Sync {
    async fn submit(&self, signed_rlp: Bytes, opts: &SubmissionOpts)
        -> Result<TxHash, SubmissionError>;
}
```

```rust
/// Per-send routing options. `Default` = public mempool (today's behavior).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionOpts {
    pub route: SubmissionRoute,
}

/// The broadcast channel for one send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmissionRoute {
    #[default]
    Public,                        // eth_sendRawTransaction, unchanged
    Private(PrivateRoute),
}

/// Private-relay routing knobs (payload of `SubmissionRoute::Private`). Public fields,
/// matching the `DiscoveryOpts` idiom. The Flashbots-only knobs (`block_window`/`fast`/
/// `hints`) on a *generic* relay are rejected at `send_with`, before the persist-before-
/// broadcast write — a clear error, never a silent submit-time drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateRoute {
    pub relay: Relay,                     // which private relay
    pub escalation: Escalation,           // REQUIRED — no Default
    pub block_window: Option<u64>,        // blocks-ahead; -> absolute maxBlockNumber at submit
    pub fast: bool,                       // MEV-Share fast inclusion
    pub hints: Hints,                     // what to reveal to searchers (rebates)
}

/// A private-relay endpoint, modeled like the RPC `Vendor` enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Relay { Flashbots, MevBlocker, Bloxroute, Custom(Url) }

/// What the bump loop does when a private tx has not landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Escalation {
    StayPrivate,                   // re-sign higher, re-send same relay
    PublicAfter { cycles: u8 },    // fall to public mempool after N bump cycles
}

/// MEV-Share disclosure flags (default = reveal nothing).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    pub calldata: bool,
    pub logs: bool,
    pub function_selector: bool,
    pub contract_address: bool,
}
```

`block_window` is stored **relative** (blocks-ahead), converted to Flashbots' absolute `maxBlockNumber = current + window` at each submit, so every bump and recovery recomputes a fresh valid window.

Two distinct failure modes kept separate: **`Fallback`** (endpoint redundancy at submit time — deferred) vs **`Escalation`** (non-inclusion over time — in the executor's bump loop).

## 5. `PrivateMev` adapter

Mirrors `PublicMempool`; selected per-send when `opts.route` is `Private`.

```rust
pub struct PrivateMev {
    flashbots: FlashbotsClient,          // alloy-mev provider + FlashbotsSignerLayer(identity)
    protect: HashMap<Relay, Arc<dyn Rpc>>, // generic Protect RPC endpoints (send-raw)
}
```

Two internal paths (the relays are not uniform):

| Path | Relays | Method | Auth | Honors `block_window`/`fast`/`hints`? |
| --- | --- | --- | --- | --- |
| Flashbots-native | `Flashbots` | `eth_sendPrivateTransaction` | `X-Flashbots-Signature` (identity key) | Yes |
| Generic Protect | `MevBlocker`, `Bloxroute`, `Custom` | `eth_sendRawTransaction` to the relay URL | none | No — Flashbots-only knobs set on a generic relay are a **build-time config error**, not silently dropped |

Generic Protect reuses the existing `Transport` pointed at the relay URL, so eRPC failover/retry/timeouts come for free.

**Reuse posture — resolved to a thin in-repo auth (see §10.1).** The reuse candidates (`alloy-mev`, `mev-share`) are a major alloy version behind our pinned `alloy 2.4.1` and cannot be git-pinned (crates.io publish forbids git deps), so reuse genuinely does not fit. We hand-roll only the small, stable `X-Flashbots-Signature` header (sign `keccak256(body)` with the identity key → `address:signature`) and call `eth_sendPrivateTransaction` over the existing `Transport`. No new deps.

**Confirmation is unchanged — H pays off here.** A privately-included tx still lands on-chain, so the executor's chain-based confirm loop settles it with no mempool visibility. The hash-anchoring correctness H proved (no false `Confirmed` under reorg/lying reads) is route-agnostic.

## 6. Executor integration

**Linchpin — `SubmissionOpts` is persisted on `TxHandle`**, exactly like the existing `#[serde(default)] cancelled` field:

```rust
pub struct TxHandle {
    // …existing fields…
    /// How this tx is broadcast. Persisted so bumps and crash-recovery re-send on the
    /// original route — a private tx must never leak to the public mempool on a bump or
    /// after a restart. Absent in pre-Phase-2 records => Public (the old behavior).
    #[serde(default)]
    pub submission: SubmissionOpts,
}
```

This is a **privacy-safety invariant** in the same family as H's confirmation-safety: absent it, recovery would default to `Public` and silently de-anonymize a private tx after a crash. The pre-Phase-2 migration is free — old records deserialize to `Public`, which is exactly what they were.

**`Router` dispatch — the executor stays route-agnostic.** It holds one `Arc<dyn SubmissionStrategy>`; when a relay identity is configured the wallet wires a `Router { public: PublicMempool, private: PrivateMev }` that dispatches on `opts.route`. No relay configured → plain `PublicMempool`, zero-cost, backward-compatible. Both submit call-sites become `self.submission.submit(rlp, &handle.submission)`; the send path sets `handle.submission = opts` before the persist-before-broadcast write; recovery/bump read it back.

**Escalation keys off the existing counter** (`handle.broadcasts.len()`, already surfaced in the bump span as `bump_count`). In `bump()`, before broadcasting:

```rust
let route = match &handle.submission.route {
    SubmissionRoute::Private(PrivateRoute { escalation: Escalation::PublicAfter { cycles }, .. })
        if handle.broadcasts.len() as u8 >= *cycles =>
    {
        warn!(intent_hash = ?handle.intent_hash, cycles, "escalating stuck private tx to public");
        handle.submission.route = SubmissionRoute::Public; // durable — recovery stays consistent, no re-hide
        SubmissionRoute::Public
    }
    other => other.clone(),
};
```

- `StayPrivate` → re-sign higher, re-send same relay, fresh block window.
- `PublicAfter { cycles }` → after `cycles` private bumps with no inclusion, rewrite the persisted route to `Public` and broadcast there. Loud WARN (trades privacy for liveness — the user opted into exactly that).

**Cancel is unchanged and honest.** The executor's existing cancel (a higher-gas 0-value self-send at the stuck nonce) routes through `handle.submission`, so a private tx's cancel also goes private (until escalated). Best-effort against the relay still including the original — but the moment *either* tx consumes the nonce the handle settles, the same "nonce consumption is ground truth" model H relies on.

## 7. Policy & error taxonomy

**Policy: nothing new lands in I — the correct call.** Route changes broadcast, not authorization: policy already gates the signature (§5.2), the route is chosen post-approval, and the identical signed tx goes out either way. A route-policy predicate now would violate define-when-needed (no consumer). One seam **documented, not built**: `hints` reveals calldata/logs to searchers — the single privacy-sensitive disclosure — and is the natural attachment point for a future `DisclosurePolicy`/`RouteAllowlist`.

**Error taxonomy — two relay variants** (the enum is already `#[non_exhaustive]`):

```rust
pub enum SubmissionError {
    Rpc(RpcError),                                          // existing
    /// Relay rejected our endpoint-auth identity (bad/rotated/expired key). NOT transient,
    /// NOT "sent" — a config error; the tx did not go out.
    RelayAuth { relay: Relay, message: String },
    /// Relay declined inclusion (profitability/simulation/policy). Terminal for this relay;
    /// the executor escalates per `Escalation` rather than assume broadcast.
    RelayRejected { relay: Relay, message: String },
}
```

**Safety invariant — H's ethic applied to broadcast:** `RelayAuth`/`RelayRejected` must never be misclassified as `is_already_accepted` ("sent"). H proved *never falsely report `Confirmed`*; the analog is *never falsely assume broadcast*. On these errors the send fails cleanly, the nonce is released (existing path), state does not advance — no phantom tracked tx that never left the process.

## 8. Testing (extends the H fault-harness)

Reuses H's `tests/support/` decorator pattern with a `RecordingRouter` mock recording `(channel, opts)` per submit. Tests target invariants, not plumbing (every test earns its place):

| Test | Proves |
| --- | --- |
| no-leak-on-recovery | persist a `Private` handle → crash → recover → re-broadcast hits **private**, never public. Privacy analog of H's no-false-`Confirmed`. |
| no-leak-on-bump | `StayPrivate` times out → every bump re-sends private (same relay) across N cycles. |
| escalation at threshold | `PublicAfter { cycles: 2 }` → bumps 1–2 private, bump 3 public, and the route rewrite **persists**. |
| no-false-broadcast | relay returns `RelayAuth`/`RelayRejected` → not recorded as sent, nonce released, state not advanced. |
| confirm parity | a privately-broadcast tx that lands confirms identically; under a reorg still never false-`Confirms` (reuses the H harness). |
| mutation check | neuter the `submission` persistence (force default-Public on recover) → no-leak-on-recovery **must fail** with a public re-broadcast. Proves the test bites. |

Relays are stubbed in-memory (no live endpoints); anvil backs confirm-parity. Each test ships with its exact single-test `cargo` command.

## 9. Footprint

- **New:** `src/core/deps/submission.rs` (types + port param + error variants — extends the existing file), `src/adapters/private_mev.rs`, `src/adapters/router.rs`, `tests/private_submission.rs`, `tests/support/` recording strategy.
- **Changed:** `src/core/wallet/primitives/handle.rs` (+`submission` field), `src/core/wallet/executor/mod.rs` (submit call-sites + escalation branch), `src/facade.rs` (`send_with`, `with_relay_identity`, Router wiring), `src/adapters/public_mempool.rs` (accept `&SubmissionOpts`), `Cargo.toml` (alloy-mev, mev-share-rs).
- **Behavior-preserving:** no relay configured ⇒ identical to today.

## 10. Open risks / plan-time gates

1. **Reuse compatibility — RESOLVED (spike, 2026-08-28): thin in-repo fallback.** `alloy-mev` newest is `1.0.0` (Sept 2025) depending on `alloy ^1.0.30` — a *major version behind* our pinned `alloy 2.4.1`; it would fork the alloy tree. `mev-share` `0.1.4` is ethers-era (2023). No alloy-2.x-compatible release exists, and a git dep is disqualifying because `walletkit-rs` publishes to crates.io (no git deps allowed). Verdict: hand-roll the `X-Flashbots-Signature` header (sign `keccak256(body)` with the identity key → `address:signature`) and call `eth_sendPrivateTransaction` over the existing `Transport`. The scheme is tiny and stable; reuse genuinely does not fit, so the house-rule fallback applies. No `alloy-mev`/`mev-share` deps are added.
2. **`serde` round-trip of `SubmissionOpts`** across all three `StateStore` backends (memory/redb/postgres) — covered by the no-leak-on-recovery test, which exercises real persistence.
3. **Generic-Protect knob rejection** is validated at `send_with`, *before* the persist-before-broadcast write — a clear `WalletKitError` when a generic relay carries Flashbots-only knobs, never a silent submit-time drop. (Pub fields keep the `DiscoveryOpts` idiom; validation lives at the send boundary, not in a builder.)

## 11. Non-goals

Bundles, submit-time `Fallback`, route-policy enforcement, `lease`/`cancel` port methods, and the J/K feature clusters. All tracked in §1 "OUT" and the Phase-2 decomposition (I → J → K).
