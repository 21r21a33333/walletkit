# Signing Surface Implementation Plan

> **For agentic workers:** implement this plan task-by-task, review-gated per `CLAUDE.md`. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a safe EIP-191 / EIP-712 signing surface — every entry point `PolicyApproval`-gated, blind-signing structurally impossible, EIP-2 low-s enforced — realizing SPEC §5.2/§5.3.

**Architecture:** A unified policy gate (`PolicyEngine::evaluate(&SigningRequest)` + native `Policy::check(&SigningRequest)`, default-deny for non-tx) with typed `Signer` methods (`sign_message`/`sign_typed_data` → `SignatureEnvelope`). Crypto is alloy's; this is the safety wrapper. See `2026-08-23-signing-surface-design.md`.

**Tech Stack:** Rust 2024, MSRV 1.85; `alloy-dyn-abi` (eip712) for `TypedData`; existing `alloy-primitives`/`alloy-signer`.

## Global Constraints

- Review-gated: implement one task, run the full gate, report **real** output, leave **uncommitted**, commit only on approval.
- Gate every task: `cargo fmt --all --check` + `cargo clippy --all-targets` + `cargo clippy --all-targets --no-default-features` + `cargo test --all-targets`. (Signing is default-features; no `postgres` needed, but keep both feature builds green.)
- No `unwrap()`/`expect()`/`panic!` in production code (allowed only in `#[cfg(test)]`).
- New public failures return `WalletKitError` (classified in `kind()`); per-port `{Trait}Error` maps in via `From`.
- New orchestration/adapter code instrumented via `crate::obs`; **`skip_all`** on every path that touches a key, approval, payload, or signature — allow-list fields only; keep the redaction test green.
- Reuse before hand-rolling: alloy for hashing/encoding/recovery/low-s; no bespoke crypto.
- Comments explain **why**, short and minimal.
- `SigningScheme` and `SigningRequest` are `#[non_exhaustive]` (grow for P256 / UserOp / 7702 later).

---

## File Structure

- `Cargo.toml` — add `alloy-dyn-abi` (eip712).
- `src/core/wallet/primitives/signing_request.rs` (**new**) — `SigningRequest`, `SigningScheme`, `SignatureEnvelope`, `SigningError`, `typed_data_hash`, `enforce_low_s`.
- `src/core/wallet/primitives/mod.rs`, `src/core/wallet/mod.rs` — re-export the new types.
- `src/core/wallet/primitives/policy.rs` — `PolicyApproval` field `intent_hash` → `payload_hash`.
- `src/core/deps/policy_engine.rs` — `evaluate(&SigningRequest)`.
- `src/adapters/policy/native.rs` — `Policy::check(&SigningRequest)`, `decide(&SigningRequest)`, new rules `MessageSigningAllowed` + `TypedDataDomainAllowlist`.
- `src/adapters/policy/wasm.rs`, `src/adapters/policy/moonpay.rs` — adapt `evaluate`; default-deny non-`Transaction`.
- `src/core/deps/signer.rs` — add `sign_message` / `sign_typed_data`.
- `src/adapters/signers.rs` — implement them + `enforce_low_s` on all paths.
- `src/core/wallet/transaction_manager.rs` — `sign_message` / `sign_typed_data` orchestration; tx `evaluate` call-site.
- `src/facade.rs` — `Wallet::sign_message` / `Wallet::sign_typed_data`.
- `src/error.rs` — `WalletKitError` signing-input variant + `From<SigningError>` + `kind()`/`remediation()`.

---

## Task 1: Signing primitives, low-s, Cargo

**Files:** Create `src/core/wallet/primitives/signing_request.rs`; modify `Cargo.toml`, `primitives/mod.rs`, `wallet/mod.rs`, `primitives/policy.rs`.

**Interfaces produced:** `SigningRequest`, `SigningScheme`, `SignatureEnvelope`, `SigningError`, `typed_data_hash`, `enforce_low_s`; `PolicyApproval::mint(payload_hash, …)` / `authorizes(payload_hash)`.

- [ ] **Step 1: Cargo** — add the dep:

```toml
# EIP-712 TypedData (dApp-supplied typed data) + Eip712Domain; reuse alloy's encoder.
alloy-dyn-abi = { version = "1", features = ["eip712"] }
```

- [ ] **Step 2: Write `signing_request.rs`.**

```rust
//! The payload model for every signing entry point. One `SigningRequest` is what the
//! policy gate authorizes and what the signer signs; `signing_hash` is the value the
//! approval binds. EIP-712 domain validation + hashing live in one place (`typed_data_hash`).

use crate::core::wallet::TxIntent;
use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, Bytes, Signature, eip191_hash_message};

/// What is being signed. `#[non_exhaustive]`: UserOp / 7702 auth / batch calls are added
/// as later phases need them, without breaking existing engines.
#[non_exhaustive]
pub enum SigningRequest {
    Transaction(TxIntent),
    /// A human-readable message; the EIP-191 `0x19` prefix is applied at hash time, so a
    /// signed message can never be a valid tx preimage (blind-signing guard, §5.2).
    Message(Bytes),
    TypedData(Box<TypedData>),
}

impl SigningRequest {
    /// The 32-byte hash the approval binds and the signer signs.
    pub fn signing_hash(&self) -> Result<B256, SigningError> {
        match self {
            Self::Transaction(intent) => Ok(intent.hash()),
            Self::Message(bytes) => Ok(eip191_hash_message(bytes)),
            Self::TypedData(td) => typed_data_hash(td),
        }
    }
}

/// EIP-712 domain validation + signing hash — the single source of truth, called both to
/// bind the approval and to sign. Rejects an absent/zero `chainId` (a domain that pins no
/// chain is a cross-chain-replay vector); exact chain/`verifyingContract` allowlisting is
/// policy's job.
pub fn typed_data_hash(td: &TypedData) -> Result<B256, SigningError> {
    match td.domain.chain_id {
        Some(id) if !id.is_zero() => {}
        _ => return Err(SigningError::ZeroChainDomain),
    }
    td.eip712_signing_hash()
        .map_err(|e| SigningError::Encode(e.to_string()))
}

/// EIP-2 low-s canonicalization — every signature this crate emits is low-s (malleability
/// guard, §5). `normalize_s` returns `Some` only when it had to flip `s`; already-low stays.
pub fn enforce_low_s(sig: Signature) -> Signature {
    sig.normalize_s().unwrap_or(sig)
}

/// Which curve/scheme produced a signature — carried in the envelope so a verifier never
/// assumes `ecrecover` (§5.3). `#[non_exhaustive]`: P256/passkey later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SigningScheme {
    Secp256k1Ecdsa,
}

/// A produced signature plus which key + scheme made it.
#[derive(Debug, Clone)]
pub struct SignatureEnvelope {
    scheme: SigningScheme,
    signer: Address,
    signature: Signature,
}

impl SignatureEnvelope {
    pub(crate) fn secp256k1(signer: Address, signature: Signature) -> Self {
        Self { scheme: SigningScheme::Secp256k1Ecdsa, signer, signature }
    }
    pub fn scheme(&self) -> SigningScheme { self.scheme }
    pub fn signer(&self) -> Address { self.signer }
    pub fn signature(&self) -> Signature { self.signature }
    /// The 65-byte `r‖s‖v` encoding dApps expect.
    pub fn as_bytes(&self) -> [u8; 65] { self.signature.as_bytes() }
}

/// A signing-input failure (distinct from a policy denial). Terminal — the caller must fix
/// the payload, not retry.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SigningError {
    #[error("EIP-712 domain has no (or zero) chainId — refusing to sign a chain-agnostic payload")]
    ZeroChainDomain,
    #[error("typed data could not be EIP-712 encoded: {0}")]
    Encode(String),
}
```

- [ ] **Step 3:** Re-export from `primitives/mod.rs` and `wallet/mod.rs`:

```rust
// primitives/mod.rs
mod signing_request;
pub use signing_request::{
    SignatureEnvelope, SigningError, SigningRequest, SigningScheme, enforce_low_s, typed_data_hash,
};
```
(and the matching `pub use` in `core/wallet/mod.rs`).

- [ ] **Step 4:** `PolicyApproval` field rename in `primitives/policy.rs` — `intent_hash` → `payload_hash` throughout (`mint` param, the struct field, `authorizes`); update the doc comment to say "bound payload" not "intent". Public method names unchanged.

- [ ] **Step 5: Tests** (in `signing_request.rs`) — only the regression-worthy logic:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_dyn_abi::TypedData;
    use alloy_primitives::U256;

    fn typed_data(chain_id: Option<u64>) -> TypedData {
        // Minimal valid EIP-712 payload; domain chainId is the variable under test.
        let json = serde_json::json!({
            "types": { "EIP712Domain": [{"name":"chainId","type":"uint256"}], "M": [{"name":"x","type":"uint256"}] },
            "primaryType": "M",
            "domain": chain_id.map(|c| serde_json::json!({"chainId": c})).unwrap_or(serde_json::json!({})),
            "message": { "x": "1" }
        });
        serde_json::from_value(json).expect("typed data")
    }

    #[test]
    fn typed_data_hash_rejects_absent_or_zero_chain() {
        assert!(matches!(typed_data_hash(&typed_data(None)), Err(SigningError::ZeroChainDomain)));
        assert!(matches!(typed_data_hash(&typed_data(Some(0))), Err(SigningError::ZeroChainDomain)));
        assert!(typed_data_hash(&typed_data(Some(1))).is_ok());
    }

    #[test]
    fn enforce_low_s_output_is_canonical() {
        // A signature whose s is already low is returned unchanged; the result is always low-s
        // (normalize_s on the output is a no-op).
        let sig = Signature::new(U256::from(1), U256::from(1), false);
        let out = enforce_low_s(sig);
        assert!(out.normalize_s().is_none(), "output must already be low-s");
    }
}
```

- [ ] **Step 6: Gate + report; leave uncommitted.**
Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo clippy --all-targets --no-default-features && cargo test --all-targets`
Commit on approval: `feat(sign): SigningRequest/SignatureEnvelope primitives + low-s + EIP-712 domain guard`

---

## Task 2: Unified policy gate

**Files:** `src/core/deps/policy_engine.rs`, `src/adapters/policy/native.rs`, `src/adapters/policy/wasm.rs`, `src/adapters/policy/moonpay.rs`, `src/core/wallet/transaction_manager.rs` (tx call-site).

**Interfaces consumed:** `SigningRequest` (Task 1). **Produced:** `PolicyEngine::evaluate(&SigningRequest)`; native rules `MessageSigningAllowed`, `TypedDataDomainAllowlist`.

- [ ] **Step 1: Broaden the port** (`policy_engine.rs`):

```rust
use crate::core::wallet::{Decision, SigningRequest};
// ...
async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError>;
```
Update the doc comment: the gate authorizes any `SigningRequest`, not only a tx.

- [ ] **Step 2: Native engine** (`native.rs`) — broaden the rule trait and fold:

```rust
pub trait Policy: Send + Sync {
    fn check(&self, request: &SigningRequest) -> Verdict;
}

impl Policy for TargetAllowlist {
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::Transaction(i) => match i.to {
                TxKind::Call(a) if self.allowed.contains(&a) => Verdict::Allow,
                _ => Verdict::Abstain,
            },
            _ => Verdict::Abstain, // tx-only rule: silent on non-tx (default-deny protects them)
        }
    }
}

impl Policy for SpendLimit {
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::Transaction(i) if i.value > self.max_value => Verdict::Deny(PolicyRejection {
                rule: "SpendLimit".into(),
                field: Some("value".into()),
                reason: format!("value {} exceeds cap {}", i.value, self.max_value),
            }),
            _ => Verdict::Abstain,
        }
    }
}
```

Add the two new rules:

```rust
/// Opt-in for EIP-191 message signing. Coarse by design: a `0x19`-prefixed message can
/// never be a valid tx preimage, so blanket message signing is low-risk. Abstains on non-messages.
pub struct MessageSigningAllowed;
impl Policy for MessageSigningAllowed {
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::Message(_) => Verdict::Allow,
            _ => Verdict::Abstain,
        }
    }
}

/// Allows EIP-712 signing only for listed `verifyingContract`s (the Permit2/Seaport guard).
/// Abstains otherwise, so unknown domains stay default-denied.
pub struct TypedDataDomainAllowlist {
    allowed: HashSet<Address>,
}
impl TypedDataDomainAllowlist {
    pub fn new(allowed: impl IntoIterator<Item = Address>) -> Self {
        Self { allowed: allowed.into_iter().collect() }
    }
}
impl Policy for TypedDataDomainAllowlist {
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::TypedData(td)
                if td.domain.verifying_contract.is_some_and(|c| self.allowed.contains(&c)) =>
            {
                Verdict::Allow
            }
            _ => Verdict::Abstain,
        }
    }
}
```

`decide` takes `&SigningRequest` and binds `request.signing_hash()`; a hash error is fail-closed as a deny:

```rust
fn decide(&self, request: &SigningRequest) -> Decision {
    let mut allowed = false;
    for p in &self.policies {
        match p.check(request) {
            Verdict::Deny(r) => return Decision::Deny(r),
            Verdict::Allow => allowed = true,
            Verdict::Abstain => {}
        }
    }
    if !allowed {
        return Decision::Deny(PolicyRejection {
            rule: "default-deny".into(), field: None,
            reason: "no policy granted permission".into(),
        });
    }
    let Ok(payload_hash) = request.signing_hash() else {
        return Decision::Deny(PolicyRejection {
            rule: "malformed-payload".into(), field: None,
            reason: "signing payload could not be hashed".into(),
        });
    };
    let valid_until = self.clock.now_unix() + self.approval_ttl;
    Decision::Allow(PolicyApproval::mint(payload_hash, self.fee_caps, valid_until))
}

#[async_trait]
impl PolicyEngine for DefaultPolicyEngine {
    async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError> {
        Ok(self.decide(request))
    }
}
```

- [ ] **Step 3: wasm + moonpay** (`wasm.rs`, `moonpay.rs`) — match on the request; keep the existing tx behavior, default-deny others:

```rust
async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError> {
    let SigningRequest::Transaction(intent) = request else {
        return Ok(Decision::Deny(PolicyRejection {
            rule: "unsupported-payload".into(), field: None,
            reason: "this engine evaluates transactions only".into(),
        }));
    };
    // ...existing per-engine logic against `intent`...
}
```

- [ ] **Step 4: tx call-site** (`transaction_manager.rs` send path) — wrap the intent:

```rust
let decision = self.policy.evaluate(&SigningRequest::Transaction(intent.clone())).await?;
```
(`TxIntent` is already `Clone`; the pipeline still owns `intent` afterward.)

- [ ] **Step 5: Tests** (`native.rs`) — update tx tests to `SigningRequest::Transaction(...)`, and add:

```rust
#[tokio::test]
async fn message_and_typed_data_are_default_denied_without_a_rule() {
    let engine = DefaultPolicyEngine::new(vec![Box::new(TargetAllowlist::new([Address::ZERO]))], Arc::new(FixedClock));
    assert!(matches!(engine.evaluate(&SigningRequest::Message(vec![1,2,3].into())).await.unwrap(), Decision::Deny(_)));
}

#[tokio::test]
async fn message_signing_allowed_grants_only_messages() {
    let engine = DefaultPolicyEngine::new(vec![Box::new(MessageSigningAllowed)], Arc::new(FixedClock));
    assert!(matches!(engine.evaluate(&SigningRequest::Message(b"hi".to_vec().into())).await.unwrap(), Decision::Allow(_)));
}

#[tokio::test]
async fn typed_data_domain_allowlist_grants_only_listed_contracts() {
    let c = Address::from([0xcc; 20]);
    let engine = DefaultPolicyEngine::new(vec![Box::new(TypedDataDomainAllowlist::new([c]))], Arc::new(FixedClock));
    // listed verifyingContract -> Allow; unlisted -> default-deny. (Build TypedData via the
    // Task-1 test helper pattern, with domain.verifyingContract = c and a non-zero chainId.)
}
```

- [ ] **Step 6: Gate + report; leave uncommitted.**
Commit on approval: `feat(policy): unified SigningRequest gate + message/typed-data rules`

---

## Task 3: Signer message + typed-data methods

**Files:** `src/core/deps/signer.rs`, `src/adapters/signers.rs`.

**Interfaces consumed:** `SigningRequest`/`SignatureEnvelope`/`enforce_low_s`/`typed_data_hash` (Task 1). **Produced:** `Signer::sign_message`, `Signer::sign_typed_data`.

- [ ] **Step 1: Port** (`signer.rs`) — add two methods (keep `sign_transaction`):

```rust
async fn sign_message(
    &self, message: &[u8], approval: &PolicyApproval, now: u64,
) -> Result<SignatureEnvelope, SignerError>;

async fn sign_typed_data(
    &self, typed: &TypedData, approval: &PolicyApproval, now: u64,
) -> Result<SignatureEnvelope, SignerError>;
```
Add `SignerError::Payload(String)` (a typed-data domain/encode failure surfaced through the signer) — or map `SigningError` in; prefer reusing `SigningError` via a `#[from]` variant:

```rust
#[error("invalid signing payload: {0}")]
Payload(#[from] crate::core::wallet::SigningError),
```

- [ ] **Step 2: LocalSigner** (`signers.rs`) — implement both; gate → hash → sign → low-s → envelope. Enforce low-s on `sign_transaction` too:

```rust
async fn sign_message(&self, message: &[u8], approval: &PolicyApproval, now: u64)
    -> Result<SignatureEnvelope, SignerError>
{
    let hash = eip191_hash_message(message);
    self.gate(approval, hash, now)?;
    let sig = enforce_low_s(self.inner.sign_hash_sync(&hash).map_err(backend)?);
    Ok(SignatureEnvelope::secp256k1(self.inner.address(), sig))
}

async fn sign_typed_data(&self, typed: &TypedData, approval: &PolicyApproval, now: u64)
    -> Result<SignatureEnvelope, SignerError>
{
    let hash = typed_data_hash(typed)?; // domain guard (single source of truth)
    self.gate(approval, hash, now)?;
    let sig = enforce_low_s(self.inner.sign_hash_sync(&hash).map_err(backend)?);
    Ok(SignatureEnvelope::secp256k1(self.inner.address(), sig))
}
```
Extract the shared gate (DRY) and reuse in `sign_transaction`:

```rust
impl LocalSigner {
    /// Bound-payload + expiry gate shared by every signing method.
    fn gate(&self, approval: &PolicyApproval, payload_hash: B256, now: u64) -> Result<(), SignerError> {
        if !approval.authorizes(payload_hash) { return Err(SignerError::ApprovalMismatch); }
        if now > approval.valid_until() { return Err(SignerError::ApprovalExpired); }
        Ok(())
    }
}
```
In `sign_transaction`, keep the fee-envelope check, reuse `gate` for the bound+expiry part, and wrap the result in `enforce_low_s`. `fn backend(e) -> SignerError::Backend`.

- [ ] **Step 3: Tests** (`signers.rs`) — behavior that can regress:

```rust
#[tokio::test]
async fn signs_message_recovers_to_signer_and_is_low_s() {
    let signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
    let msg = b"login to example.com";
    let approval = PolicyApproval::mint(eip191_hash_message(msg), GasEnvelope::DEFAULT, u64::MAX);
    let env = signer.sign_message(msg, &approval, 0).await.unwrap();
    assert_eq!(env.signature().recover_address_from_msg(msg).unwrap(), signer.address());
    assert!(env.signature().normalize_s().is_none(), "low-s");
}

#[tokio::test]
async fn sign_message_trips_the_gate_on_wrong_payload_and_expiry() {
    let signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
    let wrong = PolicyApproval::mint(B256::ZERO, GasEnvelope::DEFAULT, u64::MAX);
    assert!(matches!(signer.sign_message(b"x", &wrong, 0).await, Err(SignerError::ApprovalMismatch)));
}
```
Add a typed-data recover test (build `TypedData` as in Task 1; approval bound to `typed_data_hash`; `recover_address_from_prehash(&hash)` == address). Update `testutils::MockSigner` for the two new methods (return a canned envelope / note the call).

- [ ] **Step 4: Gate + report; leave uncommitted.**
Commit on approval: `feat(sign): Signer sign_message/sign_typed_data with low-s + gate`

---

## Task 4: Facade, orchestration, errors, observability

**Files:** `src/core/wallet/transaction_manager.rs`, `src/facade.rs`, `src/error.rs`, `src/core/wallet/signing.rs` (redaction test).

**Interfaces consumed:** all prior tasks. **Produced:** `Wallet::sign_message` / `Wallet::sign_typed_data`.

- [ ] **Step 1: Orchestration** (`transaction_manager.rs`) — mirror `send` minus gas/nonce/submit, instrumented `skip_all`:

```rust
#[cfg_attr(feature = "tracing", tracing::instrument(name = "sign_message", level = "debug", skip_all, fields(payload_hash)))]
pub async fn sign_message(&self, message: &[u8]) -> Result<SignatureEnvelope, WalletKitError> {
    let req = SigningRequest::Message(Bytes::copy_from_slice(message));
    self.authorize_and_sign(req, |signer, approval, now| {
        Box::pin(async move { signer.sign_message(message, approval, now).await })
    }).await
}
```
Because closures over `&dyn Signer` returning a future are awkward, prefer a small explicit match instead of a generic helper (keep it readable):

```rust
pub async fn sign_message(&self, message: &[u8]) -> Result<SignatureEnvelope, WalletKitError> {
    let req = SigningRequest::Message(Bytes::copy_from_slice(message));
    let approval = self.authorize(&req).await?;
    let now = self.clock.now_unix();
    Ok(self.signer.sign_message(message, &approval, now).await?)
}

pub async fn sign_typed_data(&self, typed: &TypedData) -> Result<SignatureEnvelope, WalletKitError> {
    let req = SigningRequest::TypedData(Box::new(typed.clone()));
    let approval = self.authorize(&req).await?;
    let now = self.clock.now_unix();
    Ok(self.signer.sign_typed_data(typed, &approval, now).await?)
}

/// Evaluate the gate and return the approval, or a `WalletKitError` on deny/engine error.
async fn authorize(&self, req: &SigningRequest) -> Result<PolicyApproval, WalletKitError> {
    match self.policy.evaluate(req).await? {
        Decision::Allow(approval) => Ok(approval),
        Decision::Deny(rejection) => Err(WalletKitError::Policy(rejection)),
    }
}
```
(Instrument both public methods with `skip_all, fields(payload_hash)`; set `payload_hash` from `req.signing_hash().ok()`.)

- [ ] **Step 2: Facade** (`facade.rs`) — delegate:

```rust
pub async fn sign_message(&self, message: &[u8]) -> Result<SignatureEnvelope, WalletKitError> {
    self.pipeline.sign_message(message).await
}
pub async fn sign_typed_data(&self, typed: &TypedData) -> Result<SignatureEnvelope, WalletKitError> {
    self.pipeline.sign_typed_data(typed).await
}
```

- [ ] **Step 3: Errors** (`error.rs`) — add the signing-input variant + mapping + classification:

```rust
#[error("invalid signing payload: {0}")]
Signing(#[from] crate::core::wallet::SigningError),
```
In `kind()`: `Signing => ErrorKind::Terminal`. In `remediation()`: a hint ("fix the EIP-712 domain / payload; it will never succeed as-is"). Ensure `SignerError::Payload` also flattens to this (map in `From<SignerError>`).

- [ ] **Step 4: Observability + redaction test** (`signing.rs`) — extend the redaction guard to the new paths: drive `Wallet::sign_message` / `sign_typed_data` under the capturing subscriber and assert only `payload_hash` is recorded (no key, no message bytes, no typed-data content). Reuse the existing `Capture`/`FieldSink` harness.

- [ ] **Step 5: End-to-end tests** (facade `#[cfg(test)]`): with a `DefaultPolicyEngine` carrying `MessageSigningAllowed`, `Wallet::sign_message` returns an envelope recovering to the account; with no allow rule, it returns `WalletKitError::Policy`.

- [ ] **Step 6: Gate + report; leave uncommitted.**
Commit on approval: `feat(sign): Wallet sign_message/sign_typed_data + error taxonomy + redaction`

---

## Self-review

- **Spec coverage:** §5.2 mandatory approval on every entry point (Tasks 2–4), `0x19` prefix + default-deny (Tasks 1–2), EIP-712 domain correctness (Task 1 `typed_data_hash`), low-s (Task 1/3), envelope carries key+scheme (Task 1) — all mapped.
- **Type consistency:** `payload_hash: B256` used consistently after the Task-1 rename; `SigningRequest`/`SignatureEnvelope`/`SigningError` names match across tasks; `evaluate(&SigningRequest)` signature identical in port + all engines.
- **No placeholders:** every step has concrete code or a concrete instruction referencing a prior task's helper.
- **Observability folded** into the tasks that add the code (per the standard), not a trailing task.
