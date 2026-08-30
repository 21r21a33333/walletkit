# J Phase 2 — Self-relay end-to-end: a deep dive

*Concepts this slice touched: ERC-2771 meta-transactions, the authorizer/payer split, why a
relayer is a second operated account, the EIP-712 `ForwardRequest` (signed vs calldata form),
forwarder nonces vs account nonces, error-domain fusion across a hexagonal boundary, and running
two per-account executors from one tick.*

---

## 1. The problem: acting without ETH

Every Ethereum transaction costs gas, paid in the chain's native token by **the account that
sends it**. That's a chicken-and-egg wall for onboarding: a new user has tokens they want to
*use* (a USDC balance, an NFT) but no ETH to pay for the transaction that uses them.

**ERC-2771 meta-transactions** break the link between *who authorizes an action* and *who pays
for it*:

1. The user **signs a typed message** describing the call they want (`ForwardRequest`). Signing
   is free — it never touches the chain.
2. A **relayer** (a funded account, run by the app) wraps that signed message in a real
   transaction to a **trusted forwarder contract** and pays the gas.
3. The forwarder verifies the user's signature, then calls the **target** contract, appending
   the user's address to the calldata. The target — if it's ERC-2771-aware — reads that appended
   address as `_msgSender()` instead of `msg.sender`.

So the target sees **the user** as the caller, even though **the relayer** sent and paid for the
transaction. That last-20-bytes-of-calldata trick is the whole mechanism; `_msgSender()` in an
ERC-2771 contract returns `msg.data[msg.data.length - 20:]` when `msg.sender` is the trusted
forwarder, and plain `msg.sender` otherwise.

## 2. The design insight: authorizer ≠ payer means two accounts

The naïve reading is "one wallet, gasless mode." The correct model — and the one every
production relayer (OpenZeppelin Defender, thirdweb Engine, OpenGSN, Safe) converges on — is
**two distinct operated accounts with two distinct jobs**:

| Role | Account | Signs | Sends a tx? | Nonce consumed |
| --- | --- | --- | --- | --- |
| **Authorizer** | user | the EIP-712 `ForwardRequest` (off-chain) | **never** | the *forwarder's* per-user nonce |
| **Payer** | relayer | the outer `execute()` transaction | yes | the *relayer's* account nonce |

Two nonces, two jobs. The user's **account** nonce is untouched (they sent nothing); the
**forwarder** tracks a separate per-user nonce for replay protection of forward requests. The
relayer burns its own account nonce like any normal sender.

### Why this forces a second *executor*, not just a second signer

Our tracking core, `AccountExecutor`, is bound to exactly one account: it reads
`pending_handles(self.account)` and `tx_count(self.account)`, and asserts `signer == account`.
This isn't incidental — **nonce sequencing, fee-bumping (RBF), and confirmation tracking are
intrinsically per-EOA**. Ethereum requires strictly sequential nonces per account; an
independent RBF loop and stuck-tx detector must be scoped to one sender.

The outer `execute()` tx is sent by the *relayer*. If we only swapped the signer inside the
user's manager, the resulting handle would still live under the relayer account — which the
user's executor never queries. It would never be confirmed or bumped. **The fix is composition,
not modification:** when a relayer is configured, build a *second* `TransactionManager` +
`AccountExecutor` bound to the relayer, and drive both from `Wallet::tick()`. Zero changes to
the proven reorg/bump/confirm core — we just instantiate a second copy of it. This is exactly
how thirdweb Engine ("backend wallets") and OZ Defender ("relayers") model multiple senders:
one independent per-EOA tracker each, not one account-blind loop.

```rust
pub async fn tick(&self) -> Result<(), WalletKitError> {
    self.executor.tick().await?;                 // the user account
    if let Some(gasless) = &self.gasless {
        gasless.executor.tick().await?;          // the relayer account
    }
    Ok(())
}
```

## 3. The `ForwardRequest`: two shapes of the same thing

The forwarder ABI needs **two** structs for one request, and understanding why is the crux of
EIP-712-for-meta-tx:

```solidity
// What the USER signs (the EIP-712 typehash — field order IS the type).
struct ForwardRequest      { address from; address to; uint256 value; uint256 gas;
                             uint256 nonce; uint48 deadline; bytes data; }
// What gets SUBMITTED on-chain.
struct ForwardRequestData  { address from; address to; uint256 value; uint256 gas;
                             uint48 deadline; bytes data; bytes signature; }
```

Two deliberate differences:

- **`nonce` is signed but not submitted.** The user signs *over* the nonce (so the signature is
  bound to a specific position in the replay-protection sequence), but the calldata *drops* it —
  the forwarder reads the current nonce from its own `nonces(from)` mapping and checks the
  recovered request against it. Submitting the nonce would be redundant and forgeable.
- **`signature` is submitted but not signed.** Obviously — you can't sign your own signature.

So the flow is: build `ForwardRequest` → hash it under the forwarder's EIP-712 domain → sign →
convert to `ForwardRequestData` (drop nonce, add signature) → ABI-encode `execute(data)` as the
outer calldata. We reuse alloy's `sol!` for both structs (the typehash and ABI encoding are
generated, never hand-rolled) and `TypedData::from_struct` for the EIP-712 digest.

### The domain binds the signature to *this* forwarder on *this* chain

```rust
Eip712Domain::new(Some(name), Some(version), Some(chain_id), Some(forwarder), None)
```

`verifyingContract = forwarder` and `chainId` are what stop a signature captured on one
forwarder/chain from being replayed on another. Name/version are forwarder-family-specific (OZ's
default is `"ERC2771Forwarder"`/`"1"`; a managed relay's forwarder differs), so the domain is a
small parameter the caller supplies, not a constant.

## 4. Reading the forwarder nonce: `eth_call` + a manual word decode

`forwarder_nonce` shows a small but important pattern — reading a `view` function through the
`Rpc` port and treating a **revert as a domain signal, not a transport error**:

```rust
match self.rpc.call(&request).await? {
    Simulated::Returned(bytes) => decode_forwarder_nonce(&bytes)
        .ok_or_else(|| RelayError::Forwarder { message: "…not an ERC2771Forwarder".into() }),
    Simulated::Reverted(_)     => Err(RelayError::Forwarder { message: "nonces() reverted…" }),
}
```

Two things worth internalizing:

- **A contract revert is a normal `Ok` outcome of `eth_call`**, carrying the revert data — only
  a *transport/node* failure is an `Err`. Our `Simulated::{Returned, Reverted}` enum makes that
  explicit at the port, so "the address isn't a forwarder" becomes a **terminal config error**
  (`RelayError::Forwarder`), never a retryable transient.
- **We decode the `uint256` return by hand** (`U256::from_be_slice(&ret[..32])`) rather than
  through `sol!`'s `abi_decode_returns`. A single unnamed `uint256` return *is* exactly one
  32-byte big-endian word, so the manual decode is simpler and sidesteps `sol!` return-type
  churn across alloy versions. (Reuse-first doesn't mean use-every-generated-helper; it means
  don't hand-roll what's genuinely non-trivial. A 32-byte word isn't.)

## 5. Error-domain fusion across the hexagonal boundary

This slice surfaced a subtle architectural lesson. `build_and_sign_forward_request` does two
things from different error worlds:

- reads the forwarder nonce → `RelayError` (the relay port's error);
- signs through the policy gate → `TransactionManagerError` (the pipeline's error).

A function that fuses two error domains should return the **umbrella** type, not force a lossy
mapping into one side. So it returns `WalletKitError` (which has `From` for both), and each `?`
converts at the call site. This is the same reason the **facade**, not a `Relay` adapter,
orchestrates self-relay: the outer `execute()` runs through the relayer's *full pipeline*, whose
failure surface (`nonce`/`gas`/`policy`/`store`/`submission`/…) is `TransactionManagerError` —
much richer than the Phase-1 `RelayError` (shaped for an adapter that merely *broadcasts*). Trying
to cram the pipeline's errors into `RelayError` would throw away classification. Keeping the
`Relay` port for the **managed (Gelato) HTTP** family — whose errors genuinely *are*
`RelayError`-shaped (auth/reject/transient) — is the honest boundary. **Lesson: a port's error
type should match the error surface of what implements it; when an operation spans two ports,
fuse at the umbrella, don't flatten into one.**

## 6. Confirmation safety, corrected

Phase 1 built the `ExecutedForwardRequest(signer, nonce, success)` decode expecting a mined
outer tx to sometimes carry `success = false`. Researching the OZ v5.x source corrected this:
**single `execute()` reverts** when the inner call fails, so a failed single meta-tx surfaces as
a **reverted receipt** — which H's existing logic already settles as `Failed`. The
`success = false`-on-a-mined-tx case only arises in `executeBatch()` (deferred, out of scope).
So for this slice the honest-confirm rule is simply:

- outer receipt **reverted** ⇒ `Failed` (free, via H — no meta decode needed);
- outer receipt **succeeded** ⇒ require the matching `success = true` event before `Confirmed`
  (defense-in-depth: a mined outer tx that didn't actually run *our* request must not confirm).

The relayer's executor owns this decode, because it's the executor tracking the outer tx.

## 7. What's still unproven (and why the anvil test is non-negotiable)

The unit tests use a **mock signer** whose signature is never recovered on-chain. The one thing
they *cannot* prove is that `SignatureEnvelope::as_bytes()` produces the exact 65-byte form the
forwarder's `ECDSA.recover` expects — specifically the `v` byte (27/28 vs 0/1). That's a
correctness cliff you can only see against a **real forwarder on anvil**, which is why slice B
(deploy an OZ `ERC2771Forwarder` + a trivial 2771 target, self-relay a real `execute`, assert
`Confirmed` and that the target saw the *user* as `_msgSender`) is the load-bearing proof, not a
nice-to-have. Mocks prove *wiring*; only a real chain proves *encoding*.

---

### The one-paragraph version

Gasless = split the signer from the payer. The user signs a free EIP-712 `ForwardRequest`; a
funded relayer sends and pays for the outer `execute()`. Because nonce/RBF/confirmation are
per-EOA, the relayer is modeled as a **second operated account** with its own manager+executor,
both ticked together — pure composition over the proven core, matching every production relayer.
The signed struct drops its nonce for calldata (the forwarder owns the nonce) and adds the
signature. Errors fuse at the `WalletKitError` umbrella, and the facade (not a `Relay` adapter)
orchestrates self-relay because the outer send's error surface is the full pipeline's. Mocks
prove the wiring; the anvil parity test proves the on-chain signature encoding.
