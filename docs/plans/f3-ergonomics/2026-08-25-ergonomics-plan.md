# F3 Ergonomics Implementation Plan

> Task-by-task, each a complete component; gate + commit each. Executed autonomously this run (user approved full implementation, report at end).

**Goal:** Reduce walletkit's "first tx" ceremony to alloy/ethers parity and add a policy dry-run, without hiding failure modes or the guardrail.

**Architecture:** Thin convenience wrappers over the existing infallible `Wallet::builder`; re-export modules (`prelude`/`types`/`units`); a token-free `PolicyEngine::validate` (default method + native override); runnable `examples/`. No typestate builder (retired non-goal).

## Global Constraints
- No `unwrap()`/`expect()` in production (tests/const excepted). One public error type (`WalletKitError`). Comments why-not-what. YAGNI. Reuse alloy (`parse_ether`, primitives) — never reinvent. `#[non_exhaustive]` on returned enums. Gate: `cargo fmt --check` · `cargo clippy --all-targets` · `cargo test`, green with/without `--no-default-features`. Branch `feat/ergonomics`; no Co-Authored-By.

---

### Task 1: `prelude` + `types` + `units` re-export modules

**Files:** Create `src/prelude.rs`, `src/types.rs`, `src/units.rs`; Modify `src/lib.rs`.

- `src/types.rs`: `pub use alloy_primitives::{Address, B256, Bytes, TxHash, U256};` and `pub use alloy_primitives::TxKind;` (or wherever `TxKind` resolves — verify; likely `alloy_primitives::TxKind`).
- `src/units.rs`: `pub use alloy_primitives::utils::{format_ether, format_units, parse_ether, parse_units, Unit};`
- `src/prelude.rs`: re-export `crate::{Wallet, WalletBuilder}`, `crate::core::wallet::TxIntent`, `crate::adapters::AccountManager`, the traits `crate::core::deps::{Signer, ReadClient, Rpc, PolicyEngine}`, and glob `crate::types::*`, `crate::units::*`. No adapter structs, no generic names.
- `src/lib.rs`: `pub mod prelude; pub mod types; pub mod units;` + a crate-doc line pointing at `prelude`.
- **Tests:** none (trivial re-exports). Verify by compiling; the README doctest (Task 4) exercises the prelude.

Gate → commit `feat(dx): prelude, types, and units re-export modules`.

---

### Task 2: Convenience constructors + `AllowAll` + `TxIntent` ctors

**Files:** Modify `src/facade.rs`, `src/adapters/policy/native.rs`, `src/adapters/policy/mod.rs`, `src/core/wallet/primitives/intent.rs`, `src/error.rs`.

- **`WalletKitError::Connect(String)`** (`error.rs`): a construction failure (bad URL / transport build). `kind()` → `Terminal`. No `From` needed (constructed inline).
- **`AllowAll`** (`native.rs`): `pub struct AllowAll;` impl `Policy` returning `Verdict::Allow` for every `SigningRequest`. Doc: "DEV/TEST ONLY — grants every request; never compose into a production policy." Re-export from `adapters/policy/mod.rs`.
- **`Wallet::connect_http` / `connect_http_dev`** (`facade.rs`):
  ```rust
  pub fn connect_http(url: &str, signer: impl Signer + 'static, policy: impl PolicyEngine + 'static)
      -> Result<Wallet, WalletKitError> {
      let parsed = url.parse::<url::Url>().map_err(|e| WalletKitError::Connect(e.to_string()))?;
      let transport = Transport::url(parsed).map_err(|e| WalletKitError::Connect(e.to_string()))?;
      Ok(Wallet::builder(Arc::new(transport), Arc::new(signer), Arc::new(policy)).build())
  }
  pub fn connect_http_dev(url: &str, signer: impl Signer + 'static) -> Result<Wallet, WalletKitError> {
      let policy = DefaultPolicyEngine::new(vec![Box::new(AllowAll)], Arc::new(SystemClock));
      Self::connect_http(url, signer, policy)   // reuses the one construction path
  }
  ```
  (Import `Transport`, `DefaultPolicyEngine`, `AllowAll`, `SystemClock` in facade; add `url` crate — already a dep.)
- **`TxIntent::transfer`/`call`** (`intent.rs`):
  ```rust
  pub fn transfer(chain_id: u64, account: Address, to: Address, value: U256) -> Self {
      Self { chain_id, account, to: TxKind::Call(to), value, input: Bytes::new(), purpose: None }
  }
  pub fn call(chain_id: u64, account: Address, to: Address, value: U256, input: Bytes) -> Self {
      Self { chain_id, account, to: TxKind::Call(to), value, input, purpose: None }
  }
  ```
- **Tests:** hermetic anvil — `connect_http_dev` builds a wallet that sends a tx to a non-allowlisted target (allowed because AllowAll); a `connect_http` with a `TargetAllowlist([])` denies. `TxIntent::transfer` sets the expected fields (light unit test in `intent.rs`, if it isn't pure struct-init — it maps `to`→`TxKind::Call`, worth one assertion).

Gate → commit `feat(dx): connect_http/_dev constructors, AllowAll policy, TxIntent ctors`.

---

### Task 3: `PolicyEngine::validate()` + `PolicyOutcome` + `Wallet::validate`

**Files:** Modify `src/core/wallet/primitives/policy.rs` (add `PolicyOutcome`), `src/core/deps/policy_engine.rs` (default `validate`), `src/adapters/policy/native.rs` (override), `src/facade.rs` (`Wallet::validate`), re-exports as needed.

- **`PolicyOutcome`** (`policy.rs`): `#[non_exhaustive] enum { WouldAllow, WouldDeny(PolicyRejection) }`, `#[derive(Debug, Clone, PartialEq, Eq)]`. Re-export via `core::wallet`.
- **Port default `validate`** (`policy_engine.rs`): as in the design — `evaluate` → map `Decision` → `PolicyOutcome`, dropping the approval. Documented side-effect contract.
- **Native override** (`native.rs`): refactor `DefaultPolicyEngine` so a private `decide(&req) -> Result<Verdict-ish>` core is shared; `evaluate` = decide + mint-on-allow, `validate` = decide → `PolicyOutcome` (no mint). If the existing `evaluate` already computes a `Verdict`/`PolicyRejection` before minting, extract that.
- **`Wallet::validate`** (`facade.rs`): store `policy: Arc<dyn PolicyEngine>` on `Wallet` (clone in `build()` before the executor moves it); `pub async fn validate(&self, intent: &TxIntent) -> Result<PolicyOutcome, WalletKitError>` = `self.policy.validate(&SigningRequest::Transaction(intent.clone())).await.map_err(WalletKitError::PolicyEngine)`.
- **Tests:** native `validate` returns `WouldDeny(rejection)` for a denied intent and `WouldAllow` for an allowed one, and **mints zero approvals** (assert via the same MockRpc/wallet path that `validate` doesn't broadcast — or a direct engine test that `validate` returns a token-free type and `evaluate` still denies identically). `Wallet::validate` end-to-end with `DefaultPolicyEngine`.

Gate → commit `feat(dx): PolicyEngine::validate + PolicyOutcome + Wallet::validate`.

---

### Task 4: `examples/` + doctest README + CHANGELOG

**Files:** Create `examples/{send_eth,read_balance,resolve_ens,hd_accounts,preview_and_validate}.rs`; Modify `README.md`, `CHANGELOG.md`.

- Examples: self-contained, env-driven (`WALLETKIT_RPC`, `WALLETKIT_KEY`), each `//! ` documented; must compile under `cargo build --examples`. Keep them short and real (use `prelude`, `connect_http`, `AccountManager`, `dry_run`/`validate`).
- README: replace the quickstart with a compiled ```` ```rust ```` doctest (hidden `# ` setup lines for the async runtime); ensure `cargo test --doc` passes.
- CHANGELOG `[Unreleased]`: add the F3 entry (convenience ctors, prelude/types/units, `validate`/`PolicyOutcome`, examples).

Gate (incl. `cargo build --examples` + `cargo test --doc`) → commit `feat(dx): runnable examples, doctest quickstart, changelog`.

---

## Self-Review
- **Coverage:** convenience ctors (T2), validate/PolicyOutcome (T3), prelude/types/units (T1), TxIntent ctors (T2), examples/doctests (T4). Typestate builder intentionally absent (non-goal).
- **Consistency:** `PolicyOutcome`, `Connect`, `AllowAll`, `connect_http*`, `TxIntent::transfer/call` referenced consistently.
- **Verify-at-impl:** exact `TxKind`/`Unit` import paths; whether `DefaultPolicyEngine::evaluate` already surfaces a pre-mint verdict to share with `validate`; whether `url` is a direct dep (it is).
