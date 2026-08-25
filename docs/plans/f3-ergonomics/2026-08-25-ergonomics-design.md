# F3 — Ergonomics / DX: design

**Sub-project:** F3 (last of the Phase-1 DX seams F1→F2→F3; F1 read-preview and F2 account-manager merged). **Date:** 2026-08-25. **Status:** design approved.

## 1. Goal & scope

Make walletkit pleasant to adopt. Today the "first tx" costs `3× Arc::new` + a builder — 2–3× the ceremony of alloy/ethers/viem — and callers must `use` several trait imports before any method resolves. F3 closes that gap.

**In scope (Full F3):**
- **Convenience constructors** — `Wallet::connect_http` (policy explicit) + `Wallet::connect_http_dev` (allow-all, loudly named); `TxIntent::transfer`/`call`.
- **`PolicyEngine::validate()` + `PolicyOutcome`** — side-effect-free policy dry-run (the policy analog of F1's `dry_run`); token-free by construction; `Wallet::validate`.
- **`prelude` + `types` + `units`** re-export modules.
- **`examples/` + doctest README** — runnable, compiler-checked quickstarts.

**Explicitly OUT (non-goal, research-backed):** the SPEC's "typestate `Wallet::builder()`" seam. The positional `Wallet::builder(rpc, signer, policy)` already enforces required ports at compile time (Rust API guideline C-BUILDER); a `PhantomData` rewrite would add ugly generics, unnameable builder types, and worse errors — pure ceremony. Retired as a non-goal. (If boilerplate ever bites, `bon` with `#[builder(start_fn)]` reproduces the exact public API — an internal detail, not this sub-project.)

## 2. Convenience constructors

```rust
impl Wallet {
    /// The common case: build the HTTP transport from `url`, wrap signer + policy, apply
    /// default tracking config. Policy stays explicit — the guardrail is never hidden.
    pub fn connect_http(
        url: &str,
        signer: impl Signer + 'static,
        policy: impl PolicyEngine + 'static,
    ) -> Result<Wallet, WalletKitError>;

    /// DEV/TEST ONLY — defaults to an allow-all policy. Named loudly so shipping it to
    /// production is a deliberate, visible choice, never an accidental default.
    pub fn connect_http_dev(url: &str, signer: impl Signer + 'static)
        -> Result<Wallet, WalletKitError>;
}
```

- Thin wrappers over the existing builder (one construction path underneath). `impl Trait + 'static` wrapped in `Arc` internally, so callers write no `Arc::new`.
- Fallible: parse `url: &str` → `Url` and build `Transport::url(..)` can fail. Both map into a new `WalletKitError::Connect(String)` variant (bad URL / transport build).
- `connect_http_dev` needs a blanket allow-all rule: add an `AllowAll` `Policy` in `adapters/policy/native.rs` (`Verdict::Allow` for every `SigningRequest`), with a dev-only doc warning, composed into a `DefaultPolicyEngine`.

## 3. `PolicyEngine::validate()` + `PolicyOutcome`

```rust
/// Token-free dry-run result. Structurally cannot carry a `PolicyApproval`, so it can never
/// be a signing path — the approval-minting bypass is unrepresentable, not merely discouraged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOutcome {
    WouldAllow,
    WouldDeny(PolicyRejection),
}

#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn evaluate(&self, req: &SigningRequest) -> Result<Decision, PolicyEngineError>;

    /// Side-effect-free "would this be allowed, and why?" — the policy analog of
    /// `Wallet::dry_run`. The default routes through `evaluate` and drops the approval, which
    /// is safe because `PolicyApproval::mint` is pure construction (no I/O, no state). An
    /// engine whose `evaluate` has real side effects (remote call, nonce reservation, quorum
    /// request) MUST override this with a genuinely non-minting path.
    async fn validate(&self, req: &SigningRequest) -> Result<PolicyOutcome, PolicyEngineError> {
        Ok(match self.evaluate(req).await? {
            Decision::Allow(_) => PolicyOutcome::WouldAllow,
            Decision::Deny(r) => PolicyOutcome::WouldDeny(r),
        })
    }
}
```

- **Default method** → non-breaking; existing impls (native, wasm/moonpay, `MockPolicy`) compile unchanged.
- The native `DefaultPolicyEngine` **overrides** `validate` with a genuine non-minting decide-core (evaluate = decide-core + mint-on-allow; validate = decide-core), demonstrating the correct pattern for the primary engine and avoiding even the pure-mint.
- **Explain** is met by `PolicyRejection { rule, field, reason }` on `WouldDeny` — no separate `explain()` method (matches Cedar/IAM, where "why" is a field, not a call). `WouldAllow` carries no determining-rule for now (YAGNI; add an `Explanation` only when a consumer needs it).
- **Wallet symmetry with F1:** `Wallet::validate(&intent) -> Result<PolicyOutcome, WalletKitError>` next to `Wallet::dry_run`.
- **Invariant (documented + tested):** `validate()` is advisory (TOCTOU) — a pass never short-circuits `evaluate()`; the real gate always re-runs at sign time. A test asserts `validate` mints **zero** approvals.

`Decision` is only `Allow`/`Deny` in Phase 1 (quorum is Phase 3), so `PolicyOutcome` is 2-variant. `#[non_exhaustive]` lets a `WouldRequireApproval` land later without breaking callers.

## 4. Prelude + `types` / `units` re-exports

- **`walletkit::prelude`** (small, curated, trait-forward): `Wallet`, `WalletBuilder`, `TxIntent`, `AccountManager`, the method-bearing traits (`Signer`, `ReadClient`, `Rpc`, `PolicyEngine`), plus the `types`/`units` names below. **Excludes** adapter structs (`Transport`, `LocalSigner`, `DefaultPolicyEngine` — named at construction sites via `walletkit::adapters::…`) and generic-named types (diesel's "no generic names" rule).
- **`walletkit::types`** — `pub use` of alloy `Address, U256, B256, Bytes, TxHash, TxKind`, so callers don't add `alloy` as a direct dependency (avoids version skew).
- **`walletkit::units`** — `pub use alloy_primitives::utils::{parse_ether, format_ether, parse_units, format_units, Unit}` (re-export, not reinvent).

## 5. `TxIntent` constructors

```rust
impl TxIntent {
    /// A value-only transfer to `to`.
    pub fn transfer(chain_id: u64, account: Address, to: Address, value: U256) -> Self;
    /// A contract call with `input` calldata.
    pub fn call(chain_id: u64, account: Address, to: Address, value: U256, input: Bytes) -> Self;
}
```
`purpose` defaults `None`; set it via the struct literal when needed.

## 6. `examples/` + doctest README

- Runnable `examples/`: `send_eth.rs`, `read_balance.rs`, `resolve_ens.rs`, `hd_accounts.rs`, `preview_and_validate.rs` — self-contained, RPC URL/key from env, `cargo run --example …`. Compiler-checked (`cargo build --examples`); double as smoke tests.
- Convert the README quickstart to a compiled doctest so it can't rot.

## 7. Testing (every test earns its place)

- No tests for trivial re-exports (`prelude`/`types`/`units`).
- `connect_http` builds a working wallet against hermetic anvil; `connect_http_dev` permits a tx the default-deny native engine would reject.
- `validate()` returns the right `WouldAllow`/`WouldDeny` **and mints no approval** (the key security property); `Wallet::validate` end-to-end; native override matches `evaluate`'s decision.
- `TxIntent::transfer`/`call` produce the expected fields (light).
- Examples + README doctest compile.

## 8. Task breakdown (each = one complete component)

1. **`prelude` + `types` + `units`** — re-export modules + crate-doc wiring.
2. **Convenience constructors** — `Wallet::connect_http` + `connect_http_dev` + `AllowAll` policy + `WalletKitError::Connect` + `TxIntent::transfer`/`call`.
3. **`PolicyEngine::validate()` + `PolicyOutcome`** — port default method, native non-minting override, `Wallet::validate`.
4. **`examples/` + doctest README** — runnable examples + compiled quickstart + CHANGELOG.

## 9. Research provenance

Three-agent survey (2026-08-25): (1) Rust builder/prelude/convenience-ctor conventions (bon/typed-builder/reqwest/tokio/diesel/alloy + Rust API guidelines) → verdict: keep positional builder, small trait-forward prelude, `connect` shortcut, infallible `build()`; (2) policy dry-run/explain (AWS IAM `SimulatePrincipalPolicy`, Cedar `is_authorized` diagnostics, OPA, Fireblocks Policy Inspector) → separate token-free `validate`, fold explain into the outcome; (3) wallet-SDK first-run DX (viem/ethers/alloy/web3.py) → convenience ctor + prelude + type/unit re-exports + examples are the highest value/effort. Key sources: [Rust API Guidelines C-BUILDER/C-CTOR](https://rust-lang.github.io/api-guidelines/), [diesel::prelude](https://docs.rs/diesel/latest/diesel/prelude/index.html), [Cedar Diagnostics](https://docs.rs/cedar-policy/latest/cedar_policy/struct.Diagnostics.html), [AWS IAM SimulatePrincipalPolicy](https://docs.aws.amazon.com/IAM/latest/APIReference/API_SimulatePrincipalPolicy.html), [alloy examples](https://github.com/alloy-rs/examples).
