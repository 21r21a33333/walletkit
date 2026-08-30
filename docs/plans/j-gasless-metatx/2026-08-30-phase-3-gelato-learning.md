# J Phase 3 — Managed relay (Gelato): a deep dive

*Concepts this slice touched: managed vs self-relay, fee abstraction (1Balance sponsorship vs
syncFee), sequential vs concurrent replay protection, why a hosted relay signs its **own** EIP-712
struct (and why the struct **name** is load-bearing), the build-time-credential seam, tracking a
tx you never submitted (the poll branch), and confirm-safety when the safety signal is an API
verdict rather than an on-chain event.*

---

## 1. Two ways to not pay for your own gas

Sub-project J gives a user two ways to send a transaction without holding ETH:

- **Self-relay (Phase 2):** *we* operate a funded relayer account. It submits an outer
  `execute()` through OpenZeppelin's `ERC2771Forwarder` and pays. We see every step — the outer
  tx's hash is ours the instant we broadcast it, and our own executor tracks/bumps/confirms it.
- **Managed relay (Phase 3, Gelato):** a *third party* submits and pays. We never see a mempool
  tx; we hand Gelato a signed request over HTTPS, get back an opaque `taskId`, and poll a status
  API until the task turns into an on-chain hash.

The whole design tension of this slice comes from that difference. Self-relay is "just another
operated account" — it inherits the proven core for free (Model 1). Managed relay is a genuinely
different shape: **the submitter is not us, and the thing we track is a task id, not a nonce.**

## 2. Gelato signs its own struct — and the name is the typehash

The single most important correction of Phase 3: **Gelato does not use OZ's `ERC2771Forwarder`.**
It relays through its own `GelatoRelay*ERC2771` forwarders, so the user signs a *different* EIP-712
struct bound to *Gelato's* domain. Reusing our OZ `ForwardRequest` here would produce a signature
Gelato's `ECDSA.recover` rejects — the same "the bytes only matter at the foreign boundary" lesson
from slice B, one layer up.

The struct fields:

```
sequential : chainId, target, data, user, userNonce (uint256), userDeadline
concurrent : chainId, target, data, user, userSalt  (bytes32),  userDeadline
```

Subtlety that bites: EIP-712's `typeHash = keccak256(encodeType(primaryType))`, and `encodeType`
**starts with the struct's name.** Sponsored and syncFee sequential calls have *identical fields*
but different primary-type names (`SponsoredCallERC2771` vs `CallWithSyncFeeERC2771`) — so their
typehashes differ, and the signatures are not interchangeable. In `alloy`'s `sol!`, the Rust
struct name **is** the Solidity type name, so we need four distinct structs (2 field-shapes × 2
names) even though two pairs share a layout. There is no shortcut: a macro that "reused" one struct
for both fee models would compute one typehash and Gelato would reject half the requests. Each of
the four also binds a different `verifyingContract` and domain `name`:

| fee × nonce | domain name | verifyingContract |
| --- | --- | --- |
| sponsored / seq | `GelatoRelay1BalanceERC2771` | `0xd825…a54c` |
| syncFee / seq | `GelatoRelayERC2771` | `0xb539…AE49` |
| sponsored / concurrent | `GelatoRelay1BalanceConcurrentERC2771` | `0xc65d…e816` |
| syncFee / concurrent | `GelatoRelayConcurrentERC2771` | `0x8598…d73b` |

All of this is *wire format we cannot invent* — it is pinned verbatim from `@gelatonetwork/relay-sdk`
and lives in exactly one file (`adapters/relay/gelato.rs`). The design's "risk §4" says the wire
format is fixed at implementation time from live docs; that is what the source-pinning was.

## 3. Fee abstraction: who ultimately pays

Two fee models, and they are a *credential* choice, not a per-transaction one:

- **Sponsored (1Balance):** the app pre-funds a Gelato balance and authenticates each relay with a
  `sponsorApiKey`. The user pays nothing and needs no tokens at all — the smoothest onboarding UX.
- **SyncFee:** the fee is pulled from the transaction's own ERC-20 (`feeToken`) *during* execution,
  via Gelato's relay context. No sponsor balance; the user pays, just in a token instead of ETH.

"Sequential vs concurrent" is orthogonal and is a genuine *per-request* property:

- **Sequential** uses an on-chain `userNonce` (like any nonce: request N waits for N−1). Safe,
  ordered, but two in-flight requests serialize.
- **Concurrent** uses a unique `userSalt` per request, so independent sends confirm in parallel —
  the replay guard is "this exact salt hasn't been used," not "this is the next number." The cost
  is you must guarantee salt uniqueness yourself.

## 4. The build-time credential seam

This slice's key architectural decision. A `sponsorApiKey` is an **app-level credential**, not a
per-user secret — so it is registered **once** at wallet-build time (`.gelato(Gelato::sponsored(key))`)
and never travels on a `send_gasless` call. Two forces made this the right shape:

1. **Secrets shouldn't ride on hot paths.** If the key were a per-send argument it would appear in
   every call site, every stack frame, every potential log. Registered once, it lives in one
   adapter and is redacted from `Debug`.
2. **The poller needs the relay at build time anyway.** The executor that polls the task is wired
   when the wallet is built — long before any `send_gasless`. It *must* hold the relay to poll it.

And the fact that unlocked (1) without contradiction: **Gelato's status endpoint is public.** Only
the relay POST needs the key; `GET /tasks/status/{taskId}` needs only the (non-secret) task id. So
the *same* adapter serves both — a keyed submit and a keyless poll — and nothing secret is ever
persisted on the handle. Had the status endpoint required auth, this seam would have been forced
either way; it happens to be clean.

## 5. Tracking a transaction you never submitted

Self-relay's handle is easy: we broadcast, so we have the hash immediately, and the normal
nonce-anchored confirm loop takes over. A Gelato handle is the hard case — at submit time we have
*only a task id*, no hash, no nonce we control, nothing on-chain.

The integration is a single short-circuit at the top of the confirm loop:

```rust
if handle.broadcasts.is_empty() && handle.meta.as_ref().is_some_and(|m| m.task.is_some()) {
    self.poll_task(&mut handle).await;   // Included(hash) → record it; Failed → settle; else wait
    continue;                            // skip the nonce-based transition this cycle
}
```

Two invariants make this safe to drop into an executor that is otherwise a *nonce-serialized loop
for our own txs*:

- **No signed bytes ⇒ `recover()` skips it.** The rebroadcast path guards on `!handle.signed.is_empty()`,
  so a relay-owned handle is never (uselessly, wrongly) pushed to the mempool by us.
- **No hash ⇒ the poll branch owns it, and `continue` skips the nonce logic.** The user's real
  account nonce is reconciled every cycle regardless, but the Gelato handle's own `nonce` field
  (the Gelato `userNonce`, purely for the handle id) is never compared against the chain — so a
  task handle can never be mistaken for a "foreign replacement" of the user's account.

Once the poll records a hash into `broadcasts`, the *next* cycle finds a non-empty `broadcasts`,
falls through to the ordinary `event_for` → `anchor` → depth-confirm path, and the Gelato tx
confirms exactly like any other. The managed relay adds *one branch*, not a second tracking loop —
and it reuses the entire reorg-safe, depth-gated confirm machinery unchanged.

## 6. Confirm-safety when the signal is an API verdict, not an event

Slice B's confirm-safety rule for self-relay: a mined outer `execute()` is *not* proof — decode the
forwarder's `ExecutedForwardRequest(success)` event to confirm the *inner* call ran. Gelato breaks
that, because **Gelato's forwarder emits a different event.** If `outcome_of` ran the OZ decode over
a Gelato receipt it would find no matching event and mark *every* Gelato tx `Failed` — a
catastrophic false-negative.

The fix keys on `meta.task`:

```rust
let inner_ok = match &handle.meta {
    Some(meta) if meta.task.is_none() => meta.inner_succeeded(receipt.logs()), // self-relay: decode
    _ => true, // Gelato (task=Some) or plain tx: no OZ event to decode
};
```

Why is trusting the receipt safe for Gelato? Because the honesty check has already happened
*earlier, in the poll*: we only ever record a hash on an `ExecSuccess` verdict. Gelato reports
`ExecReverted` (never a hash) when the inner call fails, and our poll maps that straight to `Failed`
without ever entering the on-chain confirm path. So by the time `outcome_of` sees a Gelato receipt,
Gelato has already vouched that the inner call succeeded — the receipt's own `status` then adds the
depth/reorg guarantee. **The safety signal simply moved from an on-chain event to the relay's task
state; the "never a false Confirmed" ethic is preserved, just enforced one step upstream.** The
`gelato_task_handle_trusts_the_relay_verdict_not_the_oz_event` test pins exactly this branch — it is
the line that would silently fail every managed tx if someone "unified" the two meta paths.

## 7. Value vs wire: where the layers split

A recurring judgment in a hexagonal codebase: what belongs in `core` and what in the adapter? Here:

- **`core` owns the contract:** `TaskId`, `RelayStatus`, the `Relay` port (reduced to just `poll`,
  because that is the *only* thing `core`'s executor needs — the facade calls the concrete adapter's
  `submit` directly, so the port never grew a method without a polymorphic consumer), and
  `MetaContext.task` (what a persisted handle must carry).
- **The adapter owns the wire:** the four `sol!` structs, the domains, the four forwarder addresses,
  the POST body field names, the status-string mapping. None of it leaks into `core`.
- **The facade orchestrates**, because it is the only layer that can see both: it asks the adapter
  to build + sign-preflight the call, routes the signing through `core`'s policy gate, hands the
  signature back to the adapter to submit, then asks `core` to persist the tracking handle.

The port shrank on contact with its first real consumer — the textbook YAGNI outcome. The committed
design sketched `relay(&SignedRequest) -> TxHandle`; the real need was `poll(&TaskId) -> RelayStatus`,
because self-relay turned out never to use the port at all and Gelato's submit isn't polymorphic.
Defining the abstraction *at* the consumer, not ahead of it, is what let it be exactly one method.

## 8. Testing a hosted SaaS you can't run locally

Anvil gave slice B a hermetic forwarder to prove signatures against. Gelato has no such thing — it
is a hosted service. So testing splits deliberately:

- **Hermetic unit tests** cover everything mechanical, behind a `GelatoTransport` seam (a two-method
  HTTP trait, stubbed in tests): the POST body carries the right nonce/salt/key for each mode and
  hits the right endpoint; the sequential nonce is read on-chain and a value-bearing intent is
  rejected before any round-trip; each task state maps to the right `RelayStatus`; the executor
  records the hash on inclusion and settles `Failed` on a drop.
- **One env-gated live test** (held until a testnet `GELATO_API_KEY` is supplied) proves the *only*
  thing a stub cannot: that the exact bytes — the four-domain typehash, the field names, the salt
  encoding — are what Gelato's real forwarder recovers and its real API accepts. Same philosophy as
  slice B's anvil test: **a mock proves the calls you make; only the foreign implementation proves
  the bytes you produce.** The PR is deliberately held on that proof.

## 9. DRY at the second use

Two adapters now POST to a relay and must classify the HTTP response identically (401/403 = auth,
5xx/429 = transient, else = parse the body). That triage — extracted from I's `classify_flashbots`
into a shared `adapters/http.rs` at exactly the moment a *second* caller appeared — is the DRY rule
applied honestly: not abstracted preemptively when Flashbots was alone, but factored out the instant
Gelato made it a duplication. The body *parsing* stays per-adapter (JSON-RPC for Flashbots, a
`taskId`/`taskState` envelope for Gelato); only the universal status split is shared.

---

### The one-paragraph version

A managed relay is a genuinely different shape from self-relay: a third party submits, so you track
a `taskId`, not a nonce. Gelato signs its own EIP-712 struct against its own forwarder — and because
the struct *name* is baked into the typehash, sponsored and syncFee need distinct structs despite
identical fields. The sponsor key is an app credential registered once at build time (safe because
the status endpoint is public, so the same adapter does a keyed submit and a keyless poll). Tracking
is one short-circuit in the confirm loop: poll until an `ExecSuccess` verdict yields a hash, record
it, and let the normal depth-confirm path finish — and confirm-safety holds because the honesty
check moved from an on-chain event to the relay's task verdict, one step upstream, never a false
`Confirmed`. The `Relay` port shrank to a single `poll` method on contact with its first real
consumer, the wire format lives in exactly one adapter, and the whole thing is unit-tested behind an
HTTP stub with one live test held back to prove the bytes against the real service.
