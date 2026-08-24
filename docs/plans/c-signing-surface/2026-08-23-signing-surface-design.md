# Sub-project C — Signing Surface (design)

**Status:** approved 2026-08-23 · **Branch:** `feat/signing-surface` · **Phase:** 1 robustness (C of A–F, +G) · **Depends on:** A (errors + observability), B (durable state), on `main`.

## Goal

Give walletkit a **safe** message/typed-data signing surface — EIP-191 `personal_sign` and EIP-712 typed data — realizing SPEC §5.2/§5.3. Every signing entry point flows through a mandatory `PolicyApproval`; blind-signing of arbitrary bytes is structurally impossible; EIP-2 low-s is enforced on every signature. The cryptographic work is alloy's (`eip191_hash_message`, `alloy_dyn_abi::TypedData`, `SignerSync::{sign_hash_sync, sign_message_sync, sign_dynamic_typed_data_sync}`, `Signature::normalize_s`); this sub-project is the **safety wrapper and policy gate**, not new crypto.

## Why now

The tx-signing path is done and gated, but a wallet library must also sign login messages (SIWE), Permit2 approvals, and other EIP-712 payloads — that is table-stakes public API. The moment such an entry point exists, the §5.2 dangers (a dApp tricking the wallet into signing a fund-draining Permit2 allowance, a malicious Seaport order, or a 7702 delegate) become real. C introduces the surface **together with** its guardrails, so the surface is never unsafe.

## Scope

**In:**
- `SigningRequest` payload model: `Transaction` (existing intent) · `Message` (EIP-191) · `TypedData` (EIP-712).
- A **unified policy gate**: `PolicyEngine::evaluate(&SigningRequest)` and the native `Policy::check(&SigningRequest)`, with default-deny for non-tx payloads.
- Typed `Signer` methods `sign_message` / `sign_typed_data` returning a `SignatureEnvelope`.
- EIP-712 domain correctness (reject `chainId` absent/zero) + EIP-2 **low-s** enforcement on all paths.
- `Wallet::sign_message` / `Wallet::sign_typed_data` public API.
- Observability + redaction on the new paths; unified `WalletKitError` classification.

**Out (deferred) — documented mapping so nothing is silently dropped:**

| Deferred item | Lands in | Kind |
| --- | --- | --- |
| 7702 authorization signing | **Phase 5** (Eip7702Delegated) | dated in SPEC |
| UserOp (V06/07/08) | **Phase 5** (ERC-4337) | dated in SPEC |
| EIP-5792 `wallet_sendCalls`/capabilities | **Phase 5** | dated in SPEC |
| P256 / passkey (secp256r1 + RIP-7212) | consumer-triggered (passkey/4337 backends) | §7 reserved seam — `SigningScheme` is `#[non_exhaustive]` |
| locked-memory / `mlock` | with a custom key backend / key generation | §7 reserved seam |
| CSPRNG fail-closed | with key/mnemonic **generation** (`AccountManager`) | §7 reserved seam — nothing random to guard yet |

## Architecture — the two-level policy model, unified per payload

The gate has two levels, and **both** broaden to `&SigningRequest`:

- **`PolicyEngine` (the port):** the pluggable engine boundary the pipeline/facade calls. Every engine implements it (`DefaultPolicyEngine`, `wasm`, `moonpay`, future Regorus). `evaluate(&SigningRequest) -> Result<Decision, PolicyEngineError>` returns `Allow(host-minted PolicyApproval)` / `Deny` / operational `Err` (fail-closed). Async. The host mints the capability, so no engine can forge authorization. This unified shape matches Turnkey "Activities" and Fireblocks TAP (one uniform request evaluated to allow / deny / require-consensus — the last being the reserved `Decision::RequireApproval` == Phase 3).
- **`Policy` (native-engine rule):** an internal detail of `DefaultPolicyEngine` only — one composable predicate returning `Verdict` (`Allow` / `Deny` / `Abstain`), folded deny-over-allow under default-deny. Sync, no capability. `wasm`/`moonpay`/Regorus do not use it.

Broadening only the port would let the native engine authorize a message/typed-data wholesale but never reason about its *content* (e.g. an EIP-712 `verifyingContract`); broadening `Policy::check` too lets built-in rules inspect the payload while `Abstain` keeps non-tx default-denied until a rule opts in.

The **Signer** stays typed (alloy/viem idiom — separate `sign_message`/`sign_typed_data`/`sign_transaction`, each reducible to a hash), not an enum-dispatch. So: **unified gate, typed signer** (the hybrid).

## Components

### `SigningRequest` (`core/wallet/primitives/signing_request.rs`, new)
```
#[non_exhaustive]
enum SigningRequest {
    Transaction(TxIntent),
    Message(Bytes),
    TypedData(Box<TypedData>),   // alloy_dyn_abi::TypedData
}
```
- `signing_hash(&self) -> Result<B256, SigningError>`: `Transaction` → `intent.hash()`; `Message` → `eip191_hash_message(bytes)`; `TypedData` → the shared `typed_data_hash` helper below.
- `SigningError` (same file): the one signing-input error — `ZeroChainDomain` (domain `chainId` absent/zero) and `Encode(String)` (typed data won't EIP-712-encode). Maps to a `WalletKitError` variant via `From` (Terminal).
- `typed_data_hash(td: &TypedData) -> Result<B256, SigningError>` (same file): the **single source of truth** for EIP-712 — reject `domain.chain_id` absent/`Some(0)` (cross-chain-replay guard), then `eip712_signing_hash()`. Both `SigningRequest::signing_hash` (to bind the approval) and the signer (to sign) call it, so domain validation lives in exactly one place.
- `Box<TypedData>` keeps the enum small; `TypedData` is a plain serde struct (`Send + Sync`).

### `SignatureEnvelope` + `SigningScheme` (same file)
```
#[non_exhaustive] enum SigningScheme { Secp256k1Ecdsa }   // P256/passkey later
struct SignatureEnvelope { scheme: SigningScheme, signer: Address, signature: Signature }
```
- `as_bytes()` → 65-byte `r‖s‖v` (via alloy `Signature`); accessors `scheme()`, `signer()`, `signature()`.
- Carries *which key + which scheme* (no `ecrecover` assumption, §5.3). No `Serialize` until a consumer needs it (YAGNI).

### `PolicyApproval` (`core/wallet/primitives/policy.rs`, change)
- Rename the bound field `intent_hash` → `payload_hash: B256`; `authorizes(hash)` unchanged. Tx still binds `intent.hash()`, so bump-reuse and the executor path are byte-identical. `gas_envelope`/`valid_until` unchanged; the envelope is consulted only on the tx path.

### Policy gate (`core/deps/policy_engine.rs`, `adapters/policy/*`)
- `PolicyEngine::evaluate(&SigningRequest)` (was `&TxIntent`); one call-site change in the pipeline (`evaluate(&SigningRequest::Transaction(intent))`).
- Native `Policy::check(&SigningRequest)`; `TargetAllowlist`/`SpendLimit` match `Transaction`, `Abstain` on the rest.
- Two new built-in rules so the surface is usable out-of-box under default-deny:
  - `MessageSigningAllowed` — coarse opt-in: `Allow` on `Message`, `Abstain` otherwise. (EIP-191 is `0x19`-prefixed → can never be a valid tx preimage, so a blanket message opt-in is low-risk.)
  - `TypedDataDomainAllowlist(HashSet<Address>)` — `Allow` on `TypedData` whose `domain.verifying_contract ∈ set`, `Abstain` otherwise.
- `wasm` / `moonpay` engines: handle `Transaction` as today, **default-deny** other variants (a plugin ABI extension for non-tx payloads is future work).

### Signer (`core/deps/signer.rs`, `adapters/signers.rs`)
- Keep `sign_transaction` (special — feeds tx assembly, returns `Signature`).
- Add typed methods returning an envelope:
  - `sign_message(&self, message: &[u8], approval: &PolicyApproval, now: u64) -> Result<SignatureEnvelope, SignerError>`
  - `sign_typed_data(&self, typed: &TypedData, approval: &PolicyApproval, now: u64) -> Result<SignatureEnvelope, SignerError>`
- Gate: `approval.authorizes(payload_hash)` + expiry (`now <= valid_until`). No fee-envelope check (tx-only).
- **EIP-712 domain correctness** is enforced by the shared `typed_data_hash` helper (single source of truth) — `sign_typed_data` calls it, so a zero/absent-chain domain is rejected before signing. Exact chain + `verifyingContract` allowlisting is the policy rule's job.
- **Low-s:** one shared `enforce_low_s(sig) = sig.normalize_s().unwrap_or(sig)`, applied on **all** paths incl. tx (defense-in-depth, §5 seam). `LocalSigner` produces the hash, gates, `sign_hash_sync`, normalizes, wraps.

### Orchestration + facade (`core/wallet/transaction_manager.rs`, `facade.rs`)
- `TransactionManager` gains `sign_message` / `sign_typed_data` (it owns policy + signer + clock): build the `SigningRequest`, `evaluate`, on `Allow` sign via the signer, return the envelope; on `Deny` return the rejection. Mirrors `send` minus gas/nonce/submit.
- `Wallet::sign_message` / `Wallet::sign_typed_data` delegate to the pipeline.

### Errors (`error.rs`)
- New `WalletKitError` variant for malformed/blind signing input — **Terminal** in `kind()`, with a `remediation()` hint. `SigningError` (zero-chain domain, typed-data encode failure) maps into it via `From`. Policy denials flow through the existing `Policy` variant. The existing `SignerError` gate variants (`ApprovalMismatch`/`ApprovalExpired`) already map in per the A-phase standard.

### Observability
- `sign_message`/`sign_typed_data` core paths: `#[instrument(skip_all, fields(payload_hash))]` — allow-list only; message bytes and typed-data content are **not** recorded (privacy + the redaction standard). Extend the existing redaction test to the new paths.

## Data flow (message / typed-data)

```
Wallet::sign_message(bytes)
  └─ TransactionManager::sign_message
       ├─ req = SigningRequest::Message(bytes)
       ├─ PolicyEngine::evaluate(&req)  →  Decision
       │     Deny  → WalletKitError::Policy(rejection)
       │     Allow(approval)  (bound to eip191_hash_message(bytes))
       └─ Signer::sign_message(bytes, &approval, now)
             ├─ gate: authorizes(payload_hash) + not expired
             ├─ sig = sign_hash_sync(eip191_hash_message(bytes))
             ├─ sig = enforce_low_s(sig)
             └─ SignatureEnvelope { Secp256k1Ecdsa, address, sig }
```
Typed-data is identical with `eip712_signing_hash` + the domain-`chainId` guard.

## Error handling

- Policy `Deny` → `WalletKitError::Policy` (Terminal). Operational engine failure → fail-closed (never sign; classified per its port error).
- Approval gate trip (wrong `payload_hash` / expired) → `SignerError::{ApprovalMismatch, ApprovalExpired}` → `WalletKitError` (Terminal).
- Malformed typed data / zero-chain domain → the new Terminal input variant.

## Testing (each earns its place)

- **Round-trip recovery:** a signed message recovers to the signer address via EIP-191; typed data via EIP-712 — proves the `0x19` prefix and domain separation are applied.
- **Default-deny:** `Message`/`TypedData` with no allowing rule → `Deny`; a `TypedData` with `chainId = 0` → rejected.
- **Low-s invariant:** every produced signature is low-s (`normalize_s()` is a no-op on the output).
- **Approval gate:** wrong `payload_hash` and expired approval each trip the gate (parallel to the tx test).
- **Redaction:** the new sign paths record only `payload_hash` — no key, no message content.
- **Policy broadening:** engine allow/deny cases for message + typed-data; existing tx tests updated for `SigningRequest::Transaction`.

No new tests for the enum plumbing or serde; the tx pipeline/executor/localnet suites are unchanged (signing is offline).

## Dependencies

- Add `alloy-dyn-abi = { version = "1", features = ["eip712"] }` for `TypedData`/`Eip712Domain` (reuse; already transitive via `alloy-signer`). No other new deps.

## Locked decisions

1. **Full safe signing surface** (message + typed-data + guardrails + envelope + low-s), not tx-path-hardening only.
2. **Secret hardening = low-s + redaction only**; `mlock`/CSPRNG-fail-closed deferred (no key generation yet; the key lives inside alloy's k256).
3. **Hybrid gate:** unified `PolicyEngine::evaluate(&SigningRequest)` + `Policy::check(&SigningRequest)`; typed `Signer` methods (not `sign(request)` enum-dispatch).
4. **7702 excluded** from C (Phase 5).
