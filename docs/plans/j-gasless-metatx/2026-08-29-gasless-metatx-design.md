# J — Gasless meta-transactions (ERC-2771): design

**Sub-project:** J (the second slice of Phase 2 — separate *who authorizes* a tx from *who
pays* for it, via ERC-2771 meta-transactions). **Date:** 2026-08-29. **Status:** designed.

> **Revision 2026-08-30 (post-Phase-1, research-backed).** Implementing Phase 2 surfaced that
> the outer `execute()` tx runs under the **relayer** account, which the per-account
> `AccountExecutor` (`self.account`, `debug_assert(signer==account)`, `pending_handles(self.account)`)
> structurally cannot track. The original "reuse the send pipeline with a swapped signer"
> assumption is unworkable. A citation-backed survey (OZ Defender Relayer, thirdweb Engine,
> OpenGSN, Safe, viem/ethers, ERC-4337) shows every self-operated relayer is a **distinct EOA
> with its own per-account tx lifecycle**. This design now adopts **Model 1: the relayer is a
> second operated account** (a second `TransactionManager` + `AccountExecutor`), sending and
> tracking the outer tx with **zero core refactor**. The relayer's outer tx runs under a
> **configurable policy (default `AllowAll`)** — the user's request is already policy-gated on
> its own signing path. See §2, §6, §7, §11, and the research note at the end.

## 1. Goal & scope

Let a user execute an action **without holding ETH**: the user signs an EIP-712
`ForwardRequest` (free), and a gas-paying relayer submits it through an `ERC2771Forwarder`
so the target contract still sees the *user* as the sender (the `_msgSender()` swap). Same
intent, same policy gate, same honest confirmation — a different *inclusion mechanism*.

**In scope:**
- **`Relay` port** (`core/deps/relay.rs`) — "get a user-signed `ForwardRequest` included by a
  gas-paying third party; return a trackable outcome." Two adapter families behind it.
- **`ForwardRequest` primitive** — the EIP-712 struct (`alloy` `sol!`/`SolStruct`), its typed
  domain, and `verify()`/nonce reads. One source of truth for the signed payload.
- **`GaslessOpts` type-state** — `SelfRelay` vs `Gelato` as distinct types (the I pattern);
  `Gelato` fee models (`Sponsored` / `SyncFee`) and nonce modes (`Sequential` / `Concurrent`).
- **`SelfRelay` adapter** — compose `execute(request)` calldata and submit the **outer tx**
  through the *existing* pipeline (so gasless composes with I's private routes and inherits
  bump/resubmit + H's confirm), signed by a **relayer** identity.
- **`Gelato` adapter** — HTTP `sponsoredCallERC2771` / `callWithSyncFeeERC2771`; returns a
  `TaskId` polled to inclusion.
- **Confirmation-safety extension (H)** — decode `ExecutedForwardRequest(signer,nonce,success)`;
  a mined outer tx with `success=false` is `Failed`, **never** `Confirmed`.
- Facade: `send_gasless(intent, opts)`, `WalletBuilder::relayer(signer)`, `.forwarder(addr)`.
- `WalletKitError::Relay(RelayError)`, classified in `kind()`.

**Explicitly OUT (deferred, tracked so it isn't lost):**
- **`executeBatch`** (atomic vs skip-invalid+`refundReceiver`) — a clean follow-up once
  single-send is proven; partial-failure/refund tracking would bloat the first cut.
- **Policy predicates** `SelectorAllowlist` / windowed `Velocity` / `check_after` — they gate
  *what may be signed at all*, not gasless specifically (a gasless request already flows
  through the existing gate as `SigningRequest::TypedData`). They belong to a **policy slice**,
  not J; folding them here couples two concerns. Noted at review as pullable if wanted.
- **Non-EIP-712 / non-2771 relaying** (raw `sponsoredCall`, OpenGSN staking network,
  ERC-4337 UserOps) — different trust/signature models; out of this slice.
- **`Gelato` `1Balance` deposit management** — an off-chain billing concern; we consume a
  sponsor key, we don't manage the balance.

**Constraint:** minimal, correctness-first, house-rule-idiomatic. **No new dependencies** —
`reqwest` (added in I), `alloy` `sol!`/`sol-types`/`dyn-abi`, and `serde` cover everything.

## 2. Why gasless is a distinct send path (not a `SubmissionRoute`)

I's `SubmissionStrategy::submit(signed_rlp, opts)` broadcasts a **raw signed tx the user
produced**. Gasless produces no such tx: the user signs a *typed message*, and a **different
key** (the relayer) produces and pays for the on-chain tx. The account nonce is untouched;
the **forwarder** nonce is consumed instead. Folding this into `SubmissionRoute` would break
the port's contract (it has no raw user tx to broadcast). So gasless is a **sibling** of
`send_with`, not a route under it — `send_gasless`. It *reuses* the submission layer for the
**outer** tx (self-relay), which is where I and J compose.

## 2a. Self-relay tracking: the relayer is a second operated account (Model 1)

The outer `execute()` tx is **sent and paid for by the relayer**, not the user. Our tracking
loop is per-account by construction — an `AccountExecutor` is bound to one `account` and only
ever reads `pending_handles(self.account)` / `tx_count(self.account)`, asserting
`signer.address() == account`. So the user's executor **cannot** track, confirm, or bump a tx
whose sender is the relayer. Threading a second signer into the user's manager does not help:
the outer handle still lives under the relayer account, which the user's executor never queries.

**Resolution — the relayer is just a second operated account.** When `relayer(signer)` +
`forwarder(addr)` are configured, the `Wallet` builds a **second** `TransactionManager` +
`AccountExecutor` bound to the relayer address, and `tick()` drives **both**. The outer tx is
sent through the *relayer's* manager and tracked/confirmed/bumped by the *relayer's* executor —
which already honors `handle.meta`, so Phase-1's confirmation-safety works **unchanged**. This
is pure composition: **no change to the core send/track/confirm code**, only a second instance
of it. The two accounts have clean roles:

| Role | Account | What it does | Nonce consumed |
| --- | --- | --- | --- |
| **Authorizer** | user | signs the EIP-712 `ForwardRequest` through the existing policy gate; **never sends** | the **forwarder** nonce (`nonces(user)`) |
| **Payer** | relayer | sends + tracks the outer `execute()` tx via its own manager/executor | the relayer's **account** nonce |

This matches every self-operated production relayer (each runs its sending EOA as a
first-class, independently-tracked account) and the viem/ethers "one nonce-manager per operated
account" idiom (see the research note). It also generalizes: the payer≠authorizer seam is the
same split ERC-4337 draws between a bundler and a smart account, so a future 4337 backend slots
in behind the same `Relay` port. Managed Gelato (§6) needs no second account at all — it *is*
the outsourced payer; its handle is task-backed, resolved by `poll`.

**Relayer policy (configurable, default `AllowAll`).** The relayer's outer tx runs under its own
policy engine, **permissive by default**: the user's request was already authorized on its own
signing path, and the relayer's spend is infrastructure, not a user action, so re-applying the
*user's* limits to it is semantically wrong. `WalletBuilder::relayer_policy(..)` lets an operator
cap the relayer (e.g. a per-tx value ceiling) when they want to; absent that, `AllowAll`.

## 3. Vocabulary alignment

Mirrors the `SubmissionOpts` idiom established in I. `Opts` = per-operation caller options;
type-state keeps invalid combinations unrepresentable rather than runtime-validated.

| Concept | Name | Precedent (I) |
| --- | --- | --- |
| per-send gasless options | `GaslessOpts` | `SubmissionOpts` (flat struct + `impl Default`) |
| relay backend choice | `GaslessRoute` | `SubmissionRoute` (domain-prefixed enum) |
| self-relay family | `SelfRelay` | `Flashbots` (family-specific struct + bare-verb builders) |
| managed family | `Gelato` | `Protect` (family-specific struct + `::vendor(..)` ctors) |
| fee / nonce method | `FeeScheme` / `NonceScheme` | `SigningScheme`, `PathScheme` (method-variant enum) |
| operation entry point | `send_gasless(intent, impl Into<GaslessOpts>)` | `send_with(intent, impl Into<SubmissionOpts>)` |

**Capability model (type-state, per house rule "make the unsafe path unrepresentable"):**
- **Concurrent (salt-based) nonce is a `Gelato` capability** — it needs a salt-aware forwarder
  (Gelato's `GelatoRelay*ERC2771`). Self-relay against the OZ standard `ERC2771Forwarder` is
  **sequential by construction**, so `NonceScheme` lives on `Gelato`, not on the common opts —
  "self-relay + concurrent" is unrepresentable, not validated.
- **`FeeScheme` (sponsor key vs fee token) is a `Gelato` capability** — self-relay's payer is the
  relayer key, so it carries no fee model. A `SyncFee` with no fee token cannot be built
  (`Gelato::sync_fee(token)` requires it).

## 4. The `Relay` port + `GaslessOpts` (`core/deps/relay.rs`)

The port abstracts the **inclusion mechanism**; the wallet owns build+sign+confirm.

```rust
/// Gets a policy-approved, user-signed `ForwardRequest` included on-chain by a gas-paying
/// third party. Self-relay submits an outer `execute()` tx we track directly; a managed relay
/// queues a task we poll. Either way the return is a `TxHandle` — tracking is uniform.
#[async_trait]
pub trait Relay: Send + Sync {
    /// Relay `signed`; returns a persisted `TxHandle` tracking inclusion.
    async fn relay(&self, signed: &SignedRequest) -> Result<TxHandle, RelayError>;

    /// Advance a task-backed handle (managed relay). Default = already settled (self-relay
    /// returns an on-chain hash synchronously); `Gelato` overrides to poll its status API.
    async fn poll(&self, _handle: &TxHandle) -> Result<RelayStatus, RelayError> {
        Ok(RelayStatus::Settled)
    }
}

/// Where a relayed request stands. `Included` hands off to the on-chain confirm path
/// (which then decodes `ExecutedForwardRequest`); `Failed` is terminal at the relay.
#[non_exhaustive]
pub enum RelayStatus { Settled, Pending, Included(TxHash), Failed(String) }
```

`SignedRequest` = the built `ForwardRequest` + its `SignatureEnvelope` (§5). Returning a
`TxHandle` (not a bare hash) unifies the two families: self-relay yields a handle already
tracking an on-chain tx; `Gelato` yields one in a task-pending state resolved by `poll`.

```rust
/// Per-send gasless options. `deadline` is common (request expiry); the relay-family choice
/// carries the family-specific knobs.
#[derive(Debug, Clone)]              // NOT Serialize — `Gelato` carries a secret (§8)
pub struct GaslessOpts {
    pub route: GaslessRoute,         // mirrors SubmissionOpts { route }
    pub deadline: Deadline,          // relative → absolute uint48 at build
}

#[derive(Debug, Clone)]
pub enum GaslessRoute { SelfRelay(SelfRelay), Gelato(Gelato) }

/// Self-relay: our funded relayer submits `execute()`. `submission` is the **outer** tx's
/// route, so gasless composes with I (gasless + Flashbots).
#[derive(Debug, Clone, Default)]
pub struct SelfRelay { pub submission: SubmissionOpts }
impl SelfRelay {
    pub fn new() -> Self { Self::default() }                       // outer tx public
    pub fn via(route: impl Into<SubmissionOpts>) -> Self { .. }   // outer tx private (I)
}

/// Managed Gelato relay. Secret-bearing → redacting `Debug` (no api key in telemetry).
#[derive(Clone)]
pub struct Gelato { pub fee: FeeScheme, pub nonce: NonceScheme }
impl Gelato {
    pub fn sponsored(api_key: impl Into<String>) -> Self { .. }   // 1Balance
    pub fn sync_fee(fee_token: Address) -> Self { .. }            // pay in ERC-20
    pub fn concurrent(mut self) -> Self { .. }                    // salt-based
    pub fn sequential(mut self) -> Self { .. }                    // #[default]
}

#[derive(Clone)] pub enum FeeScheme { Sponsored { api_key: String }, SyncFee { fee_token: Address } }
#[derive(Debug, Clone, Default)] pub enum NonceScheme { #[default] Sequential, Concurrent }

/// Request expiry as a relative window; `-> uint48` absolute (`now + window`) at build so
/// every (re)build recomputes a fresh deadline. Default: a sane short window (~1h).
#[derive(Debug, Clone)] pub struct Deadline(Duration);
```

Ergonomic `From` (mirrors I): `From<SelfRelay>`, `From<Gelato>`, `From<GaslessRoute>` for
`GaslessOpts`, so `send_gasless(intent, Gelato::sponsored(key).concurrent())` reads clean.

## 5. `ForwardRequest` primitive (`core/wallet/primitives/gasless/forward_request.rs`)

> All relayer/meta-tx primitives are grouped under the `primitives/gasless/` module
> (`forward_request.rs`, and `meta_context.rs` from §7) so the gasless domain types live in one
> discoverable place.

The single source of truth for the signed struct — built once, reused for signing,
`verify()`, and (self-relay) `execute()` calldata. **Reuse:** `alloy` `sol!` gives the
`SolStruct` (EIP-712 encoding) and event/ABI codecs for free; nothing hand-encoded.

```rust
sol! {
    // The EIP-712 *signed* type — its field order IS the typehash. `nonce` is signed here but
    // not submitted (the forwarder reads it from its own mapping).
    struct ForwardRequest {                 // matches OZ ERC2771Forwarder v5.x typehash
        address from; address to; uint256 value; uint256 gas;
        uint256 nonce; uint48 deadline; bytes data;
    }
    // The on-chain *calldata* struct — drops `nonce`, adds `signature`.
    struct ForwardRequestData {
        address from; address to; uint256 value; uint256 gas;
        uint48 deadline; bytes data; bytes signature;
    }
    function execute(ForwardRequestData request) external payable;   // self-relay
    function nonces(address owner) external view returns (uint256);
    function verify(ForwardRequestData request) external view returns (bool);
    event ExecutedForwardRequest(address indexed signer, uint256 nonce, bool success);
}
```

- **Domain** via `alloy_sol_types::eip712_domain!` — `{ name, version, chainId,
  verifyingContract: <forwarder> }`. Name/version are forwarder-specific (OZ default
  `"ERC2771Forwarder"`/`"1"`; Gelato's differ), so the domain is a small parameter carried by
  the adapter, not hard-coded.
- **Sign** by bridging to the port: `TypedData::from_struct(&req, Some(domain))` →
  `SigningRequest::TypedData` → the **existing policy gate** → `Signer::sign_typed_data`.
  Zero new signing surface; `typed_data_hash`'s zero-chain guard already protects the domain.
- **`nonce`** (sequential) read via `Rpc::call` of `nonces(from)`; **`gas`** via
  `Rpc::estimate_gas` of the *inner* call; **`deadline`** = `now + Deadline`.
- **`verify(request)`** via `Rpc::call` is the gasless analog of `dry_run` — reject a doomed
  request (bad sig / expired / untrusted forwarder) before paying to relay.

## 6. Adapters (`adapters/relay/{mod,self_relay,gelato}.rs`)

Mirrors `adapters/submission/` (a `mod.rs` re-export + one file per family).

**`SelfRelay` adapter — maximal reuse (Model 1).** Holds the **relayer's own
`TransactionManager`** (the second operated account's full pipeline: relayer signer, nonce
manager, gas oracle, submission `Router`, state store — configured with the relayer policy) and
the `forwarder` address. `relay(signed)`:
1. Compose `execute(request_data)` calldata (`sol!` `encode`) from `signed.request` +
   `signed.signature`; value = `request.value`.
2. Build the **outer** `TxIntent { account: relayer, to: forwarder, value, input }` and send it
   through the relayer manager's **existing** send path (nonce = relayer's account nonce, fees
   from its `GasOracle`, signed by the relayer, broadcast via `submission` —
   private-route-capable), which **persists** the outer `TxHandle` under the relayer account,
   stamping `meta` (§7) at build time so the confirm-safety decode is present from the first
   persist (a `send_with` variant that carries an optional `meta`, default `None` for a normal
   send). No signer threading, no account-match bypass — the relayer manager's signer *is* the
   relayer.
3. Return that handle. The **relayer's** `AccountExecutor` (ticked alongside the user's) then
   owns bump/resubmit/confirm — self-relay inherits **all** of it (this *is* the "relayer
   auto-resubmission" other services advertise). Persistence + tracking stay in the reused core,
   not the adapter.

**`Gelato` adapter — HTTP.** Reuses the `reqwest::Client` from I. `relay()` POSTs
`{chainId, target, data, user, userNonce?/salt, userDeadline, sponsorApiKey|feeToken}` +
the signature to the ERC-2771 endpoint and parses the `taskId`; the **manager** then persists
a `TxHandle` in a task-pending state (`meta.task = Some(id)`). `poll()` GETs the task-status
API, mapping
`ExecPending`→`Pending`, `ExecSuccess`→`Included(tx)`, `Cancelled`/`ExecReverted`→`Failed`.
Sequential fills `userNonce` from `nonces(user)`; concurrent omits it and sends a unique
`salt` (varied per request) for replay protection. The HTTP status-triage
(auth/transient/terminal) is a **shared adapter helper extracted from** I's `classify_flashbots`
(DRY at second use), not a copy — `Gelato` maps the triage to `RelayError` at the edge.

## 7. Executor / tracking integration + confirmation safety

**`TxHandle` gains one optional field** (same `#[serde(default)]` migration shape as I's
`submission`):

```rust
pub struct TxHandle {
    // …existing…
    /// Present iff this tx is a forwarder `execute()`. Drives the honest-confirm decode and
    /// (managed relay) task polling. Absent in pre-J records ⇒ a normal tx (old behavior).
    #[serde(default)]
    pub meta: Option<MetaContext>,
}
/// Non-secret tracking context. The Gelato api key is NEVER persisted (§8) — only what confirm
/// needs: which event to decode, and (managed) which task to poll.
#[non_exhaustive]
pub struct MetaContext {
    pub forwarder: Address, pub signer: Address, pub nonce: U256,
    pub task: Option<TaskId>,     // Some ⇒ managed relay, poll until an on-chain hash appears
}
```

**Confirmation safety — the crux (H extension).** When `meta.is_some()` and the outer tx has a
receipt, confirm decodes `ExecutedForwardRequest(signer, nonce, success)` from the logs (`sol!`
event decode, matched on `signer`/`nonce`):
- receipt **reverted** → `Failed` — H's existing logic already settles this, no meta decode
  needed. **This is the single-`execute()` failure path:** OZ `ERC2771Forwarder` v5.x reverts
  the outer tx when the inner call fails (or the request is invalid/expired), so a failed
  single meta-tx surfaces as a plain reverted receipt.
- receipt succeeded **and** a matching `ExecutedForwardRequest(signer, nonce, success == true)`
  is present → `Confirmed`.
- receipt succeeded but the matching event is **absent** (or `success == false`) → **`Failed`**.
  The `success == false`-on-a-*mined* outer tx case cannot arise for single `execute()` (it
  reverts); it is emitted only by `executeBatch()` (deferred, §1 OUT). We keep the guard anyway
  as forward-compatible defense and to reject a mined outer tx that did *not* actually execute
  our request — the meta-tx equivalent of H's "no false `Confirmed`".

So for the J scope (single `execute()`) the honest-confirm is: revert ⇒ `Failed` (free, via H);
success ⇒ require the matching `success=true` event before `Confirmed`. This slots into the
existing confirm path unchanged — hash-anchoring and reorg-safety are mechanism-agnostic; J adds
only the log decode for the `meta` case, and it runs inside the **relayer's** executor (the one
tracking the outer tx).

**Managed-relay tracking** (Gelato, no operated relayer account, no synchronous hash): the
Gelato handle is persisted under the **user** account (the authorizer) with `meta.task = Some(..)`
and no tx hash, so the **user's** executor tick picks it up, calls `Relay::poll`; on
`Included(hash)` it records the hash and the normal chain-confirm (+ the decode above) takes
over; on `Failed` it settles `Failed`. No new loop — an extra branch in the existing tick.
(Self-relay differs: its handle lives under the relayer account and is polled trivially —
`poll` defaults to `Settled` since the outer hash is already known.)

## 8. Policy, secrets, and error taxonomy

**Policy is unchanged and reused.** A gasless request is authorized by signing it as
`SigningRequest::TypedData` through the **existing** gate — gasless is not a policy bypass.
(The `SelectorAllowlist`/`Velocity` predicates that would *further* restrict it are a separate
policy slice, §1 OUT.) The **outer** self-relay tx is a normal pipeline send and passes the
relayer account's own policy.

**Secret handling (differs from I — important).** Unlike `SubmissionOpts` (no secrets, fully
persisted), `GaslessOpts` carries the Gelato **api key**. Therefore:
- `GaslessOpts`/`Gelato`/`FeeScheme` are **not** `Serialize` and get a **redacting `Debug`**
  (the api key never reaches a log/span/handle). The key lives in the `Gelato` adapter, built
  once at wiring time.
- Only the **non-secret** `MetaContext` (forwarder/signer/nonce/task) is persisted — exactly
  what confirm/poll need, nothing more. Redaction test extended to cover `Gelato`.

**Error taxonomy** (`RelayError`, `#[non_exhaustive]`, maps into `WalletKitError::Relay`,
classified in `kind()`):

```rust
pub enum RelayError {
    Rpc(#[from] RpcError),                 // nonce/verify/estimate/self-relay submit — reuses I classes
    Submission(#[from] SubmissionError),   // self-relay outer broadcast (relay-auth/rejected/etc.)
    Signing(#[from] SignerError),          // request signing (gate trip / payload)
    /// Managed relay rejected the request (bad sig, unsupported chain, sponsor exhausted).
    /// Terminal — the request did not enter a task. `message` carries the relay's reason.
    Rejected { message: String },
    /// The forwarder cannot be used as configured (not a contract / doesn't trust target /
    /// unknown domain) — a config error surfaced by `verify()` before paying.
    Forwarder { message: String },
}
```
`kind()` **delegates** to the inner error's existing predicates (`RpcError`'s transient flag,
`SubmissionError::is_transient`/`is_relay_terminal`) — it never re-matches strings; the
`Rpc`/`Submission` split is by call-site (direct forwarder reads vs the outer broadcast), not
duplication. `Rejected`/`Forwarder` = `Terminal`; a task that later `Failed` settles the handle
`Failed` (not an `Err`). No relay failure is ever mistaken for "sent/confirmed" — H/I's ethic
carried forward.

## 9. Testing (extends the H/I harness; every test earns its place)

| Test | Proves |
| --- | --- |
| forward-request-hash | the `ForwardRequest` EIP-712 hash matches an OZ `ERC2771Forwarder` fixture (pin a `cast`/known vector) — the signed bytes are wire-correct. |
| no-false-confirm-on-inner-revert | outer tx mined, `ExecutedForwardRequest.success=false` ⇒ handle `Failed`, never `Confirmed`. **The J invariant** (H analog). |
| success-confirms | `success=true` ⇒ `Confirmed`; event absent ⇒ `Failed`. |
| self-relay-composes-private | self-relay with `SelfRelay::via(Flashbots..)` submits the **outer** tx on the private route (records the channel) — gasless + I compose. |
| gelato-task-lifecycle | stubbed status API: `ExecPending`→Pending, `ExecSuccess`→`Included(hash)`→chain-confirm, `Cancelled`→`Failed`. |
| gelato-secret-redaction | `format!("{:?}", Gelato::sponsored(k))` never contains the key; api key not on the persisted handle. (Extends the redaction test.) |
| nonce-mode | sequential fills `userNonce` from `nonces`; concurrent omits it + varies `salt`. |
| verify-preflight | a request `verify()` says is invalid is rejected before any relay/submit. |
| anvil confirm-parity | over anvil + a deployed OZ `ERC2771Forwarder`: a real self-relayed `execute` confirms honestly; reorg → no false `Confirmed` (reuses the H `FaultRpc`). |

In-memory relay/forwarder stubs for unit tests; anvil + the real forwarder for the parity
test (hermetic — no external endpoints). Each ships its exact single-test `cargo` command.

## 10. Footprint

- **New:** `core/deps/relay.rs` (port + `GaslessOpts` type-state + `RelayError`),
  `core/wallet/primitives/gasless/` (grouped relayer primitives: `forward_request.rs` =
  `sol!` struct + typed-data/verify/nonce; `meta_context.rs` = `MetaContext`),
  `core/wallet/gasless.rs` (read helpers + `send_gasless` orchestration),
  `adapters/relay/{mod,self_relay,gelato}.rs`, `tests/gasless.rs`, test-support relay/forwarder
  stubs.
- **Changed:** `core/wallet/primitives/handle.rs` (+`meta`), `core/wallet/primitives/mod.rs`
  (exports), `core/wallet/executor/mod.rs` (confirm decode + task-poll branch — used by the
  relayer's executor for self-relay and the user's for Gelato),
  `core/wallet/transaction_manager.rs` (build-sign of the `ForwardRequest` on the user manager +
  an internal `meta`-carrying send used by the relayer manager for the outer tx; `send`/`send_with`
  keep their public signatures and behavior), `facade.rs` (`relayer`, `forwarder`,
  `relayer_policy`; **build a second `TransactionManager` + `AccountExecutor` for the relayer
  account and `tick()` both**; `send_gasless` orchestration + adapter wiring), `error.rs`
  (`Relay` variant + `kind()`), `adapters/mod.rs` (module wiring), `testutils.rs` (relay mock).
- **No `Cargo.toml` change** — reuse `reqwest`, `alloy sol!`/`sol-types`/`dyn-abi`, `serde`.
- **Behavior-preserving:** no relayer/forwarder configured ⇒ a `Wallet` is exactly as before
  (one account, one executor); `send`/`send_with` byte-identical; pre-J handles deserialize with
  `meta = None`. The second account/executor exists **only** when a relayer is configured.

## 11. Open risks / plan-time gates

1. **Reuse posture — no new crate.** No published crate wraps ERC-2771 forwarders on our pinned
   `alloy 2.4.1`; the whole struct/domain/event/ABI surface is already provided by `alloy`
   `sol!` (the *same* reuse we lean on in `read`/`multicall`). Hand-rolling would re-encode
   EIP-712 by hand — rejected. Verdict: `sol!` + `sol-types` + `dyn-abi::TypedData::from_struct`.
2. **Outer-tx signing + tracking identity (resolved: Model 1, §2a).** Self-relay's outer
   `execute` must be signed **and tracked** under the relayer account. An earlier idea —
   thread a relayer signer into the user's manager and pick it when `meta.is_some()` — is
   **rejected**: it fixes signing but not tracking (the user's per-account executor never
   queries the relayer's handles), and it bypasses the `signer==account` invariant. Instead the
   relayer is a **second operated account**: its own `TransactionManager` (relayer signer,
   configurable policy) + `AccountExecutor`, both driven by `Wallet::tick()`. Zero core change,
   the `signer==account` invariant holds for both accounts, and confirm-safety is inherited.
3. **`meta` round-trips** across all three `StateStore` backends — covered by the confirm tests
   over real persistence.
4. **Gelato wire format** (exact field names, salt encoding, status enum strings) is pinned at
   adapter-implementation time from the live API docs; the adapter is the only place that
   knows the wire shape (`sol!`/domain confined likewise).

## 12. Non-goals

`executeBatch`; the `SelectorAllowlist`/`Velocity`/`check_after` policy predicates (policy
slice); ERC-4337 UserOps / OpenGSN staking / raw non-2771 relaying; 1Balance deposit
management; a generic multi-backend `ExecutionBackend` abstraction (J ships two concrete
families; the abstraction earns its place only when a third backend appears).

## 13. Research note — how production relayers model the sender (2026-08-30)

Surveyed before adopting Model 1; every **self-operated** relayer models its sender as a
distinct EOA with its own per-account tx lifecycle (nonce, RBF, confirmation), never one
account-agnostic loop. Only a **fully-outsourced** relay (Gelato) hides the sender behind a
task id — which is exactly our Gelato adapter, not self-relay.

| System | Relayer = separately-tracked account? | Source |
| --- | --- | --- |
| OZ Defender Relayer | Yes — dedicated EOA; own nonce/RBF/resubmission | <https://docs.openzeppelin.com/defender/module/relayers> |
| thirdweb Engine | Yes — "backend wallets", endpoints scoped `/{chain}/{walletAddress}/` | <https://portal.thirdweb.com/changelog/engine-nonce-management-improvements> |
| OpenGSN | Yes — RelayWorker EOAs, "each worker independent with its nonce mgmt" | <https://github.com/opengsn/gsn/issues/232> |
| Safe relay service | Yes — relayer EOA submits `execTransaction` | <https://github.com/5afe/safe-relay-service> |
| Gelato Relay | No — outsourced; poll a `taskId` (`getTaskStatus`) | <https://docs.gelato.cloud/web3-services/relay/relay-api> |
| ERC-4337 (contrast) | Bundler submits ≠ account authorizes; tracked by userOpHash | <https://eips.ethereum.org/EIPS/eip-4337> |

Client-lib idiom: viem attaches a `nonceManager` **per local account**
(<https://viem.sh/docs/accounts/local/createNonceManager>); ethers' `NonceManager` wraps a
single signer. "One nonce-manager per operated account" is the norm — Model 1 mirrors it.

OZ `ERC2771Forwarder` v5.1.0 (source-confirmed,
<https://github.com/OpenZeppelin/openzeppelin-contracts/blob/v5.1.0/contracts/metatx/ERC2771Forwarder.sol>):
`event ExecutedForwardRequest(address indexed signer, uint256 nonce, bool success);`. Single
`execute()` **reverts** on inner failure; only `executeBatch()` emits `success=false` on a mined
tx. `_checkForwardedGas` prevents relayer gas-griefing, so a `success=false` is a genuine
app-level revert. Drives §7's confirm rule.

**Forward-compat:** Model 1's payer≠authorizer seam is the same split ERC-4337 draws between a
bundler and a smart account; a future 4337 backend fits behind the `Relay` port, provided
`TxHandle` stays an **opaque status token** (outer hash / taskId / userOpHash) — the thirdweb/
Gelato lesson.
