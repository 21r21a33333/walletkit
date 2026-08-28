# Learning — Phase 1: the routing seam

A deep dive on the concepts this phase touched. The code is small; the ideas behind it are the point.

## 1. The Strategy pattern at a port boundary

`SubmissionStrategy` is a textbook **Strategy pattern**: a single operation (broadcast a signed tx) with interchangeable implementations (`PublicMempool`, later `PrivateMev`), chosen at runtime. What makes it powerful *here* is that the strategy sits at a **port** — a trait behind `Arc<dyn …>` that the core pipeline depends on abstractly. The executor calls `submission.submit(rlp, opts)` and has no idea whether the bytes go to a public mempool, a Flashbots relay, or a test recorder.

The phase's whole job was to widen that operation from `submit(rlp)` to `submit(rlp, opts)` **without the pipeline learning anything new**. That's the tell of a good seam: a capability (per-tx routing) was added, and the code that orchestrates sends (`TransactionManager`, `AccountExecutor`) changed only by passing one more argument. The knowledge of *what the routes are* lives entirely in the adapter layer.

Why not push routing into the transport (the RPC connection)? Because a transport knows about *connections*, not *policy*. Routing is a decision — "this tx should be private" — that we want visible to policy, observability, and persistence. Encoding it as an argument to the strategy keeps it a first-class, inspectable value rather than a hidden property of a socket.

## 2. `Opts` vs `Config`: two kinds of "settings"

Rust codebases accumulate settings structs, and it's worth being deliberate about which kind you're writing. This repo draws a sharp line:

- **`Config`** = build-time infrastructure wiring, usually deserialized from a file or env: `TransportConfig` (endpoints, retries), `FinalityConfig` (confirmation depth). You set it once when constructing a component.
- **`Opts`** = per-operation caller choices, usually with a `Default`: `DiscoveryOpts` (how to scan for accounts), and now `SubmissionOpts` (how to route *this* send).

The distinction matters because it tells the reader *when* a value is decided and *who* decides it. `SubmissionOpts` is an `Opts` because routing is chosen per-`send()`, by the caller, with a sensible default (public). Getting the vocabulary right isn't pedantry — it's how a new reader predicts the shape of the API before reading the body. When the user pushed back on "Preferences" (a word the repo never uses), that was the real lesson: **match the codebase's existing idiom over your own defaults**, because consistency is a feature.

## 3. Forward-compatible enums: `#[non_exhaustive]` and `#[default]`

Two attributes did a lot of work:

```rust
#[derive(Default)]
pub enum SubmissionRoute {
    #[default]
    Public,
    Private(PrivateRoute),
}
```

`#[default]` on a unit variant lets the enum derive `Default`, which lets the *containing* struct (`SubmissionOpts`) derive `Default` too — and that default is load-bearing. It's why a legacy persisted record with no `submission` field, or any caller who doesn't care, transparently routes public. The default *is* the backward-compatibility story.

```rust
#[non_exhaustive]
pub enum Relay { Flashbots, MevBlocker, Bloxroute, Custom(Url) }
```

`#[non_exhaustive]` tells downstream crates "this enum will grow" — they cannot write a `match` without a wildcard arm, so adding `Relay::Blink` later is not a breaking change for them. Inside our own crate, we still match exhaustively (the attribute only binds *other* crates). This is the library-author's discipline: every public type that models an open-ended domain (relays, error kinds, signing schemes) is `#[non_exhaustive]` from day one, because the alternative is a major version bump every time reality expands.

## 4. Where to put validation: boundary vs type-state

There are two ways to make an invalid state (a generic relay carrying Flashbots-only knobs) impossible:

1. **Type-state** — make the illegal combination unrepresentable. A builder where `.block_window()` only exists on a `FlashbotsRoute` type. Airtight, but heavier machinery, and it fights the repo's pub-field `Opts` idiom.
2. **Boundary validation** — keep the struct permissive (pub fields, like `DiscoveryOpts`) and reject bad combinations at the one door they must pass through: `send_with`, before the persist-before-broadcast write.

We chose (2). The trade-off is honest: a caller *can* construct a nonsensical `PrivateRoute`, but it cannot *send* one — `validate()` fails the send cleanly before any irreversible step (nonce allocation, signing, broadcast). The rule of thumb: reach for type-state when the invalid value is dangerous to merely *hold*; reach for boundary validation when it's only dangerous to *act on*, and the ergonomic cost of type-state isn't worth it. Routing knobs are the latter — an unused `fast: true` sitting in a struct harms nothing until you try to broadcast it.

This is also why `validate()` is a **pure function** returning `Result<(), RouteError>`. Pure validators are trivially testable (no I/O, no mocks — the phase's one `submission` unit test is three assertions) and composable: the same check runs at `send_with` in Phase 2 without change.

## 5. The dispatch combinator

`Router` is a **combinator** — a strategy built from other strategies:

```rust
match &opts.route {
    SubmissionRoute::Public  => self.public.submit(rlp, opts).await,
    SubmissionRoute::Private(_) => self.private.submit(rlp, opts).await,
}
```

Two design choices worth noticing. First, `Router` holds `Arc<dyn SubmissionStrategy>` for *both* arms, not concrete types — so it doesn't depend on `PrivateMev` (which doesn't exist yet). A combinator should know the *interface* it composes, never the concrete implementations; that's what let this land in Phase 1 while the private adapter waits for Phase 2. Second, `Router` is wired **only when a private relay is configured**. No relay → the wallet uses `PublicMempool` directly, so the common path pays zero dispatch cost and behaves byte-for-byte as before. Combinators are opt-in overhead; don't impose them on users who don't need the composition.

## 6. Domain primer: why private submission exists

When you broadcast to the public mempool, your transaction sits in the open where anyone can see it before it's mined. For a large swap, searchers can **front-run** (buy ahead of you, selling into your price impact) or **sandwich** (buy before, sell after) — value extracted from you, called MEV (Maximal Extractable Value).

Private submission routes around the public mempool:
- **Protect RPCs** (Flashbots Protect, MEV Blocker, bloXroute) accept your signed tx over a private channel and hand it directly to block builders. It never hits the public pool, so there's nothing to front-run.
- **`eth_sendPrivateTransaction`** adds knobs: `maxBlockNumber` (give up after block N), `fast`, and MEV-Share **hints** — deliberately revealing *some* of your tx (calldata, logs) so searchers can *backrun* you and share the profit back as a rebate. Disclosure is a dial, not a switch.
- **Order-flow auctions** (MEV Blocker, CoW) auction the right to backrun your tx and rebate you the proceeds — turning MEV from a tax into a discount.

The `Hints` struct models exactly this disclosure dial, and the fact that it's the one privacy-sensitive knob is why the design flags it as the future attachment point for a disclosure policy.

## 7. What Phase 1 deliberately left as scaffolding

The seam is in, but nothing routes privately yet — and that's correct sequencing. `Router`, `PrivateRoute`, `Relay`, `RouteError`, and the persisted-route field (Phase 2) are the *shape* the feature will fill. This is the payoff of "one complete component per phase": Phase 1 is complete and verifiable (the public path is provably unchanged, the new types are tested) even though the user-visible feature arrives in Phase 2. A seam you can gate green on its own is worth more than a half-wired feature that needs the next phase to compile.

The single biggest idea carried forward: **the route must be persisted with the transaction**, because a bump or a crash-recovery re-broadcasts, and re-broadcasting a private tx on the public mempool would silently de-anonymize it. Phase 1 built the type (`SubmissionOpts: Serialize + Default`) precisely so Phase 2 can persist it as one `#[serde(default)]` field — the same trick the existing `cancelled` flag uses. Design the data so the correctness property becomes a one-line, backward-compatible addition later.
