# Phase 1 learning — J (gasless meta-tx)

Deep dives on the concepts each Phase-1 step touches. Written as I implement, for mastery.

---

## Step 1 — EIP-712 and the `ForwardRequest` primitive

### The problem this step actually solves

A meta-transaction has to be *signed by the user* but *submitted by someone else*. The only
thing the user contributes is a signature over a message. For that signature to be safe, the
message must be:

1. **Unambiguous** — it can never be mistaken for a *different* message (or, worse, for a real
   transaction the user didn't mean to authorize).
2. **Bound to one place** — a signature valid on Ethereum mainnet must not also be valid on
   Polygon, or against a *different* forwarder contract, or replayable a second time.
3. **Human-inspectable** — a wallet UI should be able to show the user *what* they're signing
   in structured, named fields, not an opaque 32-byte blob.

EIP-712 ("typed structured data") is the standard that delivers all three. Step 1 builds the
one type that represents this signed message — `ForwardRequest` — and the function that turns
it into the exact 32 bytes that get signed.

### EIP-712 from the ground up

A raw ECDSA signature signs a 32-byte hash. The whole game is: *how do you compute that hash
from a structured message so the three properties above hold?* EIP-712 defines it as:

```
digest = keccak256( 0x19 ‖ 0x01 ‖ domainSeparator ‖ hashStruct(message) )
```

Three ingredients:

**1. The `0x1901` prefix.** The leading `0x19` byte is the EIP-191 "this is not RLP" marker —
it guarantees the preimage can never be a valid RLP-encoded transaction, so a signed *message*
can never be replayed as a signed *transaction*. The `0x01` version byte says "structured data
follows." (This is the same family of guard as our existing `eip191_hash_message`, which uses
the `0x19` prefix for `personal_sign`.)

**2. `hashStruct(message) = keccak256( typeHash ‖ encodeData(message) )`.**
   - `typeHash = keccak256("ForwardRequest(address from,address to,uint256 value,uint256 gas,uint256 nonce,uint48 deadline,bytes data)")`.
     This string — the *type* of the struct, fields in declared order — is hashed once. It is
     what makes the message unambiguous: change a field name, type, or order and the typeHash
     changes, so a signature for one struct shape can't be reinterpreted as another.
   - `encodeData` concatenates each field encoded to 32 bytes: value types (`address`,
     `uint256`, `uint48`) are left-padded big-endian words; *dynamic* types (`bytes`, `string`)
     are replaced by their `keccak256` hash. That's why in the hand-computation the `data` field
     becomes `keccak256(0xdeadbeef)` before it goes into the struct hash.

   Subtlety worth internalizing: **`uint48 deadline` is encoded as a full 32-byte word**, the
   same as a `uint256` numerically. The 48-bit width is a *storage/ABI* constraint on-chain
   (a `uint48` timestamp is plenty and packs tightly), but in EIP-712 encodeData every numeric
   member is one 32-byte word. That's why the `cast abi-encode` cross-check passes `uint256`
   for the deadline slot and still matches.

**3. `domainSeparator = keccak256( eip712DomainTypeHash ‖ keccak256(name) ‖ keccak256(version) ‖ chainId ‖ verifyingContract )`.**
   This is property (2) — *bound to one place*. `chainId` stops cross-chain replay;
   `verifyingContract` (the forwarder address) stops a signature meant for forwarder A being
   replayed against forwarder B; `name`/`version` namespace the app. Our existing
   `typed_data_hash` already refuses to sign a domain with an absent/zero `chainId` for exactly
   this reason — a chain-agnostic domain is a replay vector.

Notice what is *not* in the domain or the struct's replay story yet: the **nonce**. In
ERC-2771 the `nonce` is a *field of the signed struct* (so two otherwise-identical requests
produce different signatures), but it is **not** submitted in calldata — the forwarder reads
the expected nonce from its own `nonces(from)` mapping and consumes it. So the nonce gives
*sequential* replay protection (each signature is good exactly once, in order), while the
domain gives *spatial* replay protection (right chain, right contract). Two orthogonal guards.

### Why reuse `alloy`'s encoder instead of hand-rolling

Everything above is mechanical and easy to get *subtly* wrong (a mis-ordered field, forgetting
to hash the dynamic `bytes`, padding a `uint48` wrong). alloy's `sol!` macro generates, from
the Solidity-shaped struct declaration, a Rust type that implements `SolStruct` — which knows
its own `typeHash` and `encodeData`. `TypedData::from_struct(&req, Some(domain))` then assembles
the full EIP-712 `TypedData` (domain + type resolver + message), and `eip712_signing_hash()`
produces the `0x1901…` digest. We write the struct shape *once*, in the same declarative form
as the Solidity contract, and never touch the byte layout. This is the house "reuse before
hand-rolling" rule at its highest-leverage: the alternative is re-implementing a
consensus-critical hash by hand.

The one bridge detail: `from_struct` needs `S: SolStruct + Serialize`, because it stores the
message as JSON (`serde_json::Value`) for inspectability. So the `sol!` struct carries
`#[derive(serde::Serialize)]` — the macro recognizes the serde derive and emits a field-named
serialization that matches the EIP-712 member names. (This works because
`alloy-dyn-abi`'s `eip712` feature pulls in `alloy-sol-types/eip712-serde`.)

### Testing methodology — the "independent oracle" cross-check

The test pins a **golden digest computed by `cast`** (Foundry) — a completely separate EIP-712
implementation — for a fixed request and the OZ domain:

```
digest = 0xd1882449115c3e37d2347d4a36df523be018bc2479caa841c413ccdf345c6ddb
```

Why this is a *real* test and not a tautology: if I asserted our `from_struct` hash equalled
our own `SolStruct::eip712_signing_hash`, I'd only be proving alloy is self-consistent — a bug
in *my field order or domain wiring* would appear on both sides and cancel out. By computing the
expected value with a foreign tool (`cast keccak` + `cast abi-encode`, following the EIP-712
formula by hand), any mistake in *my* declaration — wrong field order, wrong domain
`name`/`version`, a `uint48` mis-encoding — makes the two diverge. Agreement across two
independent implementations is strong evidence the wire format is correct. This mirrors the
house rule "when asserting real-chain values, compute expected with `cast`, never approximate."

### YAGNI note carried into the code

The `sol!` block defines *only* `ForwardRequest` right now, not the rest of the forwarder ABI
(`execute`/`nonces`/`verify`/the `ExecutedForwardRequest` event). Generated-but-unused call and
event types would trip the zero-warning clippy gate, and they have no consumer yet — each grows
in the exact step that first uses it (nonce/verify reads in Step 2, the event decode in Step 4).
"Define at first consumer" applies even to macro-generated surface.

---

## Step 3 — the `Relay` port and type-state options

### What a "port" is, and why this one returns a `TxHandle`

In hexagonal architecture a *port* is the interface the core depends on and adapters
implement. `Relay` is the gasless inclusion port: "take a signed `ForwardRequest`, get it
on-chain by someone who pays, and give me something I can track." The subtle design choice is
the *return type*. The two adapter families produce very different artifacts — self-relay
yields a real on-chain tx hash immediately, while a managed relay (Gelato) yields only a *task
id* that later resolves to a hash. If the port returned `TxHash`, the managed case couldn't
honor it. If it returned an enum `{Hash | Task}`, every caller would branch. Instead it returns
a `TxHandle` — the same lifecycle object the rest of the wallet already tracks — so *tracking is
uniform* and the task-vs-hash difference is hidden inside `poll` (a defaulted method: synchronous
relays inherit "already settled," managed relays override). This is "program to the widest
common abstraction the callers actually need," and it's why `poll` has a default body: most of
the trait's surface costs an implementor nothing.

### Type-state: making the illegal unrepresentable

The house rule "encode invariants in the type system — make the unsafe path *unrepresentable*"
shows up here as the shape of `GaslessOpts`. Two capabilities — the fee model (`Sponsored`
vs `SyncFee`) and the nonce strategy (`Sequential` vs `Concurrent`) — exist **only** for the
managed Gelato relay. The OZ standard `ERC2771Forwarder` has no salt path, so "self-relay with
a concurrent nonce" is *meaningless*, and "sync-fee without a fee token" is *malformed*.

A naive design carries booleans/options on one flat struct:

```rust
struct GaslessOpts { concurrent: bool, fee_token: Option<Address>, sponsor_key: Option<String>, self_relay: bool }
```

— and now you must *validate at runtime* that `self_relay && concurrent` is rejected, that
`SyncFee` implies `fee_token.is_some()`, etc. Every one of those checks is a bug waiting to be
forgotten. The type-state alternative pushes the knobs onto the family that owns them:

```rust
enum GaslessRoute { SelfRelay(SelfRelay), Gelato(Gelato) }
struct Gelato { fee: FeeScheme, nonce: NonceScheme }          // knobs live here
enum FeeScheme { Sponsored { api_key }, SyncFee { fee_token } }  // token is *inside* the variant
```

Now `SelfRelay` structurally cannot hold a `NonceScheme`; `SyncFee` cannot exist without a
`fee_token`. The compiler enforces what would otherwise be a runtime `validate()`. The
constructors (`Gelato::sponsored(key).concurrent()`, `SelfRelay::via(route)`) read like prose
and can only build *valid* states. This is the exact same move sub-project I made with
`Flashbots` vs `Protect` — the second time we've reached for it, which is a good sign the
pattern is load-bearing, not incidental.

### The redacting `Debug` — a security invariant in a trait impl

`FeeScheme::Sponsored` holds a Gelato sponsor api key: a secret. The repo rule is absolute — *no
secret ever reaches a log, span, error, or `Debug`*. The trap is that `#[derive(Debug)]` would
happily print the key, and `Debug` output ends up in `tracing` fields and panic messages. So
`Gelato` and `FeeScheme` get a **hand-written `Debug`** that prints `api_key: "<redacted>"`, and a
test asserts a known key never appears in the formatted output. Two things make this robust:
(1) it's *allow-list* framing — we write out exactly the safe fields, so a *new* secret field
added later is omitted by default rather than leaked by default; (2) the test is a regression
guard, so if someone "simplifies" it back to a derive, CI goes red. This is also why
`GaslessOpts` is deliberately **not** `Serialize`: a serialized options blob would carry the key
into a persisted handle. Only the non-secret tracking context is ever persisted (Step 4's
`MetaContext`).

### The sequencing lesson: a step is only "done" if it's gate-green *alone*

Step 2 (the forwarder-read helpers) was *planned* before Step 3, but implementing it first would
have failed our own gate. A `pub(crate)` function consumed only by a `#[cfg(test)]` test still
counts as **dead code in the plain library build** (the test module is `cfg`-stripped there), and
`cargo clippy --all-targets` compiles that plain build — so it warns, and zero-warnings is the
gate. The same is true of the `sol!`-generated `nonces`/`verify` call types: unused codegen warns.

The fix wasn't an `#[allow(dead_code)]` (that hides the smell); it was recognizing the *real*
dependency order. The helpers need `RelayError` (Step 3) to classify a forwarder revert, and they
need a live consumer (`send_gasless`, Phase 2) to not be dead. So Step 3 — the port and
type-state, which are **public API** and therefore never dead-code-warn even without implementors
— moves first, and the helpers move to Phase 2 beside their consumer. The general principle:
*internal* code must be pulled into existence by a consumer; only *public API* may stand alone.
When a "helper-first" step has no consumer yet, that's the signal it belongs with its consumer.

---

## Step 4 — confirmation safety for meta-transactions

### The lie a mined transaction can tell

The whole tracking system rests on one signal: a transaction is `Confirmed` when its receipt
mines at depth with EIP-658 status `1`. For a *normal* tx that's the truth — status `1` means
the call the user signed executed. **For a meta-transaction it is a lie.** The relayer signs
and mines `forwarder.execute(request)`. The forwarder verifies the signature, consumes the
nonce, and *then* calls the user's target. If that inner call reverts, the forwarder catches
it, emits `ExecutedForwardRequest(signer, nonce, success=false)`, and **returns normally** — so
the *outer* tx has receipt status `1`. A naive tracker would mark the handle `Confirmed`, and
we'd have told the user their action succeeded when it reverted. This is precisely the failure
sub-project H spent itself preventing ("never a false `Confirmed`"), reappearing through a new
door. The only ground truth is the *event*, not the receipt status.

### Where the fix goes — and why not in the state machine

The executor is a *functional core / imperative shell*. The core is `transition(state, event)`
— a pure FSM, exhaustively table-tested, that maps a distilled `ChainEvent::Mined { outcome }`
to the next `TxStatus`. The shell (`AccountExecutor`) does the I/O and *distills* each unreliable
chain read into one trustworthy event.

The tempting-but-wrong move is to teach the FSM about meta-transactions. That would pollute a
pure, decision-making core with decoding logic and a new dependency on the forwarder ABI. The
right seam is where the *outcome is decided from the receipt* — `anchor()` in the shell. The FSM
already knows "`Reverted` ⇒ `Failed`, `Executed` ⇒ `Confirmed`"; all that has to change is the
*computation of `Outcome` from a receipt*, and that computation is exactly a shell concern
(it reads chain data). So the entire change is one pure helper:

```rust
fn outcome_of(receipt, handle) -> Outcome {
    let inner_ok = match &handle.meta {
        Some(meta) => meta.inner_succeeded(receipt.logs()), // decode the event
        None => true,                                       // a normal tx: status is truth
    };
    if receipt.status() && inner_ok { Executed } else { Reverted }
}
```

The FSM is untouched; its 15 lifecycle tests still pin every reorg/depth rule. The new logic is
a small, independently-testable function. This is the payoff of the core/shell split: a
genuinely new *evidence-gathering* rule slots into the shell without touching the *decision*
rules. When you feel the urge to add a special case to a pure core, first ask whether it's
really about *what the evidence is* (shell) rather than *what to decide given the evidence*
(core).

### Decoding an event, precisely

`MetaContext::inner_succeeded` finds the `ExecutedForwardRequest` **matching this request** —
same `signer` and `nonce` — and returns its `success`; a missing or mismatched event is `false`
("not proven" ⇒ fail-safe). Matching on `signer`+`nonce` matters because a single outer tx
could, in principle, carry several forwarder events (batching); we settle on *ours*. The decode
reuses alloy's `SolEvent::decode_log_data` on each `LogData` — no hand-parsed topics — the same
"reuse the generated codec" discipline as the EIP-712 signing side.

### The persistence shape — a zero-cost migration

`meta` is `Option<MetaContext>` with `#[serde(default)]`, exactly like I's `submission`. A
pre-J or non-gasless handle deserializes to `meta = None`, which flows through `outcome_of` as
"a normal tx" — so old records behave identically and the schema grows without a migration
script. `MetaContext` is `#[non_exhaustive]` so its `task` field can be added in Phase 3 without
breaking callers, and it is deliberately *non-secret*: the Gelato api key never lands on a
persisted handle, only the forwarder/signer/nonce the confirm path actually needs.

### Testing the invariant without a chain

Two focused tests pin it. `inner_succeeded_only_on_a_matching_success_event` builds receipts'
logs with `encode_log_data` and asserts the decode is correct for success/failure/absent/
mismatch — the four cases that matter. `gasless_outer_success_does_not_confirm_a_reverted_inner_call`
proves the shell wiring: an outer-success receipt with *no* proving event yields `Reverted` for
a `meta` handle but `Executed` for a plain one. Together with the FSM's existing "`Reverted` ⇒
`Failed` at depth" test, that composes into the full invariant — no live chain required. The
end-to-end proof over a real deployed forwarder (which emits the real event) is Phase 2's anvil
parity test; here we prove each link in isolation.

---

## Step 5 — folding a new error into the public taxonomy

Every `Wallet` operation returns exactly one error type, `WalletKitError`, with a
machine-readable `kind()` (`Retryable` / `Terminal` / `NeedsReconcile`) callers branch on. A new
port error has to answer three questions: how does it *reach* the umbrella, how is it
*classified*, and does it need a *remediation* hint.

The classification is where the DRY discipline matters. `RelayError` wraps two errors that
*already* have a classification — `RpcError` (transient vs not) and `SubmissionError` (its
`is_already_accepted`/`is_transient`/`is_underpriced` predicates). The wrong move is to
re-inspect their messages in `relay_kind`; the right one is to **delegate**:

```rust
fn relay_kind(e: &RelayError) -> ErrorKind {
    match e {
        RelayError::Rpc(e)        => rpc_kind(e),          // reuse
        RelayError::Submission(e) => submission_kind(e),   // reuse
        RelayError::Signing(_) | RelayError::Rejected { .. } | RelayError::Forwarder { .. }
                                  => ErrorKind::Terminal,
    }
}
```

So a transient RPC failure *inside* a relay op is `Retryable` for the same reason it is anywhere
else — one source of truth for "what does a timeout mean," reused rather than re-encoded. The
only genuinely new judgements are the relay-specific terminals: a managed relay declining the
request, or a forwarder that isn't a conforming `ERC2771Forwarder` — both permanent, the latter
worth a `remediation()` hint ("check the forwarder address and that the target trusts it").

The variant is added now even though the *constructor* (the gasless send path) arrives later.
That is not dead code: `WalletKitError` is public API, so its variants and the `From<RelayError>`
impl are part of the surface downstream callers compile against — the same "public API may stand
alone" rule from Step 3. What is *deferred* is the internal plumbing
(`TransactionManagerError`/`ExecutorError` gaining a `Relay` arm), because those are internal and
must be pulled into existence by the code that actually produces a `RelayError`.


