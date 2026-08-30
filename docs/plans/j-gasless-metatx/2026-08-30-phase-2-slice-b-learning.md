# J Phase 2 slice B — Proving a meta-transaction on a real chain: a deep dive

*Concepts this slice touched: why an on-chain test is the only proof of signature encoding, the
ECDSA recovery id (`v`) and the 27/28-vs-0/1 cliff, how OpenZeppelin's `ERC2771Forwarder`
validates a `ForwardRequest`, deploying + reading contracts from a Rust integration harness,
reproducing a reorg deterministically, and how to write an integration test that cannot pass by
accident.*

---

## 1. The one thing mocks cannot prove

Slice A shipped the whole self-relay pipeline and proved its *wiring* with mock signers and a
mock RPC: build the `ForwardRequest`, sign it, compose `execute()` calldata, route it through the
relayer's manager, stamp `meta`. Every unit test passed. Yet one property stayed completely
unproven, because a mock signer's signature is never *recovered by anyone*:

> Does `SignatureEnvelope::as_bytes()` produce the exact 65 bytes the forwarder's on-chain
> `ECDSA.recover` expects?

This is not a wiring question — it's an *encoding* question, and encoding is only observable at
the boundary where the bytes are interpreted by a **different** implementation. Our signer
produces bytes; only a real `ecrecover` consuming them can tell us whether the layout is right.
A mock that hands the bytes back to our own code proves nothing: both sides could share the same
bug and still agree.

This is the general lesson: **mocks verify the calls you make; only a foreign implementation
verifies the bytes you produce.** Any time your code emits a wire format that something else
parses — a signature, an ABI blob, a serialized message, a protocol frame — a test that both
produces and consumes it with *your* code is circular. The real proof needs the other end.

## 2. The `v` byte: a one-byte cliff

Ethereum signatures are 65 bytes: `r` (32) ‖ `s` (32) ‖ `v` (1). `r` and `s` are the ECDSA
signature proper; `v` is the **recovery id** — the extra bit that lets `ecrecover` reconstruct
*which* of the two possible public keys (and thus which address) produced the signature, without
being given the key.

The cliff: there are two conventions for `v`.

| Convention | Values | Where it comes from |
| --- | --- | --- |
| Raw parity | `0` / `1` | the y-coordinate parity the curve math yields |
| Ethereum `ecrecover` | `27` / `28` | parity **+ 27**, fixed by the Yellow Paper / Homestead |

The `ecrecover` precompile — and therefore OpenZeppelin's `ECDSA.recover` built on it — accepts
**only 27/28**. Hand it a `v` of `0`/`1` and it does not error helpfully; it returns
`address(0)` (or, in OZ's wrapper, reverts as an invalid signature). So a signer that emits raw
parity produces signatures that *look* structurally valid — right length, right `r`/`s` — but
recover to the wrong (zero) address. Every downstream check that compares the recovered signer
to the expected one then fails, often far from the real cause.

Our chain of custody:

```
Signer (alloy) → SignatureEnvelope::secp256k1 → as_bytes() → execute_calldata(signature)
```

`as_bytes()` delegates to `alloy_primitives::Signature::as_bytes()`, which (in 1.x) writes
`sig[64] = 27 + y_parity` — i.e. it already normalizes to Ethereum's 27/28. So the encoding
*should* be right. "Should" is exactly what an integration test converts into "is". The happy-path
test ran the real forwarder's `ECDSA.recover` over our bytes; the forwarder recovered the user,
matched it against `request.from`, and executed. **The slice-A ⚠️ is now resolved with zero
production change** — the bytes were already correct, and now we *know* it instead of hoping.

## 3. How the OZ `ERC2771Forwarder` validates a request (and where our test bites)

Walking OZ v5.x `execute(ForwardRequestData)` → `_execute(request, requireValidRequest: true)`:

1. **`msg.value == request.value`** — the outer tx's ETH must match the request's declared value
   (we send `value: intent.value`, and the intent is 0 here).
2. **`_validate`** recovers the signer from the EIP-712 digest of the `ForwardRequest` and returns
   four flags. The three that gate execution:
   - `isTrustedForwarder` — the *target* must trust this forwarder (our `RecordingTarget`'s
     constructor took the forwarder address).
   - `active` — `block.timestamp <= request.deadline` (our 3600s window).
   - `signerMatch` — `recovered == request.from`. **This is the `v`-byte gate.**
3. On any false flag with `requireValidRequest`, `execute()` **reverts** (`InvalidSigner`,
   `ExpiredRequest`, `UntrustfulTarget`). A single `execute()` never mines a "soft failure".
4. Otherwise it consumes the forwarder's per-user nonce, calls the target with
   `abi.encodePacked(request.data, request.from)` — appending the user's 20 bytes so the target's
   ERC-2771 `_msgSender()` reads *the user* — runs `_checkForwardedGas`, and emits
   `ExecutedForwardRequest(signer, nonce, success)`.

Our happy-path test bites at **every** one of these:

- wrong `v` → `signerMatch` false → revert → receipt status 0 → our executor settles `Failed`, not
  `Confirmed`. The `assert Confirmed` catches it.
- wrong calldata suffix / a target that doesn't trust the forwarder → `_msgSender()` returns the
  forwarder or the relayer, not the user. The `assert last_sender == user` catches it (and since
  user ≠ relayer, "sees user" is a real discriminator, not trivially true).
- inner call never ran → `pokes` stays 0. The `assert pokes == 1` catches it.

That's what makes the test **non-vacuous**: for each way the encoding or wiring could be wrong,
a *named* assertion flips. A test that can only pass is worthless; a good integration test is one
you can point at a specific bug and say "this line would go red."

## 4. Why the reverting-inner case is caught *before* signing

The natural expectation from the plan was "execute-revert → Failed handle". But our architecture
makes that path unreachable for a *deterministic* revert, and that's a feature. `build_and_sign_
forward_request` estimates the inner call's gas with `eth_estimateGas` — which **executes** the
call in the node's simulated state. A call that always reverts (our `boom()`) fails estimation
with a non-transient error, which we map to `WalletKitError::Simulation` **before signing and
before the relayer sends anything**.

So the honest assertion is not "the outer tx mined and reverted" — it's "the relayer never spent
a nonce on a doomed request". The test proves that with `onchain_tx_count(relayer) == 0`. This is
the fail-fast gate protecting the payer: a relayer that blindly submitted every request would burn
real ETH on reverts. (A *state-dependent* revert — one that passes estimation but reverts at mine
time — would still produce a mined reverted receipt → `Failed`; that path is covered by the
executor's own reverted-receipt unit tests, so re-proving it here would earn no new coverage.)

## 5. Deploying and reading contracts from a Rust harness

Two small but reusable techniques:

**Committing creation bytecode.** Real chains want *creation* bytecode (constructor + a copy of
the runtime code); `anvil_setCode` wants *runtime* bytecode. We deploy, so we commit the creation
`.bin` (from `forge inspect <C> bytecode`) and send it as a tx with no `to`. To keep the committed
artifact self-contained, the forwarder fixture is a **no-arg subclass**:

```solidity
contract Forwarder is ERC2771Forwarder {
    constructor() ERC2771Forwarder("ERC2771Forwarder") {}
}
```

so the creation code needs no appended constructor args. The target *does* take one (`address
forwarder`), so the harness appends it by hand — a constructor arg is just ABI-encoded bytes glued
to the end of the creation code, and an `address` is a left-padded 32-byte word:

```rust
code.extend_from_slice(&[0u8; 12]);
code.extend_from_slice(forwarder.as_slice());
```

**Reading `view`s without an ABI binding.** Rather than pull in a typed contract binding, the
harness does a raw `eth_call` and decodes the single-word return by hand — a `uint256` is one
big-endian 32-byte word; an `address` is its low 20 bytes:

```rust
let out = self.eth_call(target, /* pokes() selector */).await;   // 32 bytes
U256::from_be_slice(&out)
```

This mirrors the same "don't hand-roll what's non-trivial, but a 32-byte word is trivial"
judgment the production `decode_forwarder_nonce` makes.

## 6. Reproducing a reorg deterministically

The most important safety property of the whole library is **never a false `Confirmed`**. A
naïve chain can't be *forced* to serve a receipt from a block it has reorged away — an honest
node either has the block or doesn't. So we reproduce that one adversarial condition with a
`FaultRpc` decorator that lies on exactly the read a false confirm hinges on: `block_hash(n)`.

The executor confirms a receipt only if it can **anchor** it — re-fetch `block_hash(receipt.
block_number)` and check it still equals the receipt's block hash (a reorg would change it). Make
`block_hash` return a bogus hash and the anchor check fails: the receipt is treated as
no-evidence, and the handle stays un-confirmed no matter how deep it's "mined". Clear the fault
and the *same* handle confirms — proving it was confirmable all along and the **guard**, not an
unconfirmable tx, was holding it.

For slice B the point is that this guard is the **relayer executor's** job now. The meta handle
lives under the relayer account (it sent the outer `execute()`), so it's the relayer's executor
that must anchor it. The test drives the gasless wallet's `tick()` — which fans out to both
executors — and asserts the relayer-owned meta handle honours confirm-safety exactly like a plain
tx. Model 1's payoff, restated as a test: the relayer is *just another operated account*, so it
inherits every safety property of the proven core for free.

## 7. What earns a place in an integration suite

Three tests, each proving something no cheaper test can:

1. `self_relay_confirms_and_target_sees_the_user` — the encoding + attribution + confirm proof.
   Only a real forwarder can give it.
2. `reverting_inner_is_rejected_before_signing` — the fail-fast payer guard, against a real
   revert (a mock can't revert *plausibly*).
3. `reorg_of_the_outer_execute_never_falsely_confirms` — confirm-safety composes with the relayer
   executor + `meta`.

Everything else about gasless — the decode logic, the two `ForwardRequest` shapes, the error
fusion — is already pinned by unit tests. We deliberately did **not** re-run the storage-backend
matrix here: confirm-safety is storage-agnostic, so three more anvil spins over redb/Postgres
would add cost and no new failure mode. That restraint is part of "every test earns its place":
the question is never "could this pass?" but "what bug does this, and only this, catch?"

---

### The one-paragraph version

Mocks prove the calls you make; a real chain proves the bytes you produce. The bytes in question
were the signature's `v` — 27/28, not 0/1 — and only the genuine OZ `ERC2771Forwarder`'s
`ECDSA.recover` could confirm we got it right; it did, so slice A's warning closes with no code
change. The happy-path test is non-vacuous because a wrong `v`, a broken calldata suffix, or an
un-run inner call each flip a *named* assertion. A deterministically-reverting inner call is
caught by the pre-sign gas-estimate gate, so the relayer never spends a nonce — the fail-fast
payer guard. And because Model 1 makes the relayer just another operated account, its executor
inherits the never-false-`Confirmed` reorg guard, which we reproduced deterministically with a
`block_hash`-lying `FaultRpc` rather than a real reorg.
