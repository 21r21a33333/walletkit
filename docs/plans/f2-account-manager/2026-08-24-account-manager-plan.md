# F2 AccountManager Implementation Plan

> **For agentic workers:** implement task-by-task. Each task builds one **complete component**, ends with the gate run, and stops **uncommitted for review**; commit only on explicit approval, then start the next task (per `CLAUDE.md`). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add an HD account-management layer to walletkit — generate a BIP-39 seed, derive multiple accounts under it, discover used accounts, predict counterfactual smart-account addresses, and import/export encrypted keystores.

**Architecture:** A seed-owning concrete adapter `AccountManager` (`adapters/accounts.rs`) over pure domain types (`core/accounts/primitives.rs`). It mints `LocalSigner`s that plug into today's `Wallet::builder` (no facade change). Reuses `coins-bip39`/`coins-bip32` (derivation), `alloy` `MnemonicBuilder`/keystore/`Address::create2`, and the F1 `ReadClient`/`Rpc` ports (discovery + deployed-check).

**Tech Stack:** Rust, alloy 2.4.1, alloy-signer-local (keystore+mnemonic), coins-bip39 0.12, coins-bip32 0.12, rand 0.8, zeroize.

## Global Constraints

- **No `unwrap()`/`expect()` in production code** — propagate via `?` / `AccountError`; allowed only in `#[cfg(test)]`.
- **One public error type:** fallible public APIs return `WalletKitError`; `AccountError` is the internal port-style contract, mapped in via `From` and classified in `kind()`.
- **Secrets:** mnemonic/seed handled fail-closed (OS CSPRNG, error not panic), wrapped in `zeroize::Zeroizing`, never `Clone`d, never logged; every secret-bearing type has a redacting `Debug`. No raw-mnemonic reveal API.
- **Reuse before hand-rolling:** `Address::create2`, `MnemonicBuilder`, alloy keystore, `public_key_to_address` — never re-implement.
- **Named returns, not positional tuples.** `#[non_exhaustive]` on returned structs/enums.
- **Comments why-not-what**, minimal. **Every test earns its place** — no struct-init/serde/config tests.
- **Gate before stopping (report real output):** `cargo fmt --check` · `cargo clippy --all-targets` (zero warnings) · `cargo test`. Green with **and without** `--no-default-features`.
- **Branch:** `feat/account-manager`. **No Co-Authored-By trailer.** Confirm before every push. Update `CHANGELOG.md` `[Unreleased]` before the PR.

## File Structure

- `src/core/accounts/mod.rs` — module re-exports (Create).
- `src/core/accounts/primitives.rs` — pure types + path helpers + `predict_address` (Create).
- `src/core/mod.rs` — add `pub mod accounts;` (Modify).
- `src/adapters/accounts.rs` — `AccountManager` + discovery (Create).
- `src/adapters/mod.rs` — add `pub mod accounts;` + re-exports (Modify).
- `src/adapters/signers.rs` — add `from_mnemonic_path`, `export_keystore`, `import_keystore` on `LocalSigner` (Modify).
- `src/error.rs` — add `WalletKitError::Account` + `From` + `kind()` (Modify).
- `src/lib.rs` / `README.md` / `CHANGELOG.md` — surface + docs (Modify, Task 6).
- `Cargo.toml` — add `coins-bip39`, `coins-bip32`, `rand` (Modify, Task 1).
- `tests/accounts.rs` — integration (derivation vectors, discovery vs anvil, keystore round-trip, predict) (Create, across tasks).

---

### Task 1: Core primitives, PathScheme & dependencies

**Files:**
- Create: `src/core/accounts/mod.rs`, `src/core/accounts/primitives.rs`
- Modify: `src/core/mod.rs`, `Cargo.toml`

**Interfaces:**
- Produces: `PathScheme{Bip44Standard,LedgerLive,Custom(String)}`, `WordCount{W12,W15,W18,W21,W24}`, `Account{index:u32,path:String,address:Address,label:Option<String>}`, `AccountError`, and helpers `PathScheme::path_for(&self, index:u32) -> Result<String, AccountError>`, `WordCount::entropy_len(&self)->usize` / `word_count(&self)->usize`.

- [ ] **Step 1: Add dependencies to `Cargo.toml`** (under `[dependencies]`, pinned to alloy's transitive versions to avoid duplicates):

```toml
# HD account derivation (same versions alloy-signer-local pins) — Mnemonic/XPriv/XPub.
coins-bip39 = { version = "0.12", default-features = false, features = ["english"] }
coins-bip32 = "0.12"
# OS CSPRNG for fail-closed mnemonic entropy (matches coins-bip39's rand major).
rand = "0.8"
```

- [ ] **Step 2: `src/core/accounts/primitives.rs`** — pure types + path helper + error. Real code:

```rust
//! Pure account-management domain types: HD path schemes, derived-account records,
//! and the counterfactual-address predictor. Zero I/O.

use alloy_primitives::Address;

/// Which BIP-44 slot the `index` shortcut varies. The two schemes agree only at index 0
/// and produce entirely different address sets otherwise — a notorious interop footgun.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathScheme {
    /// `m/44'/60'/0'/0/{index}` — MetaMask/viem/ethers/alloy default (varies the address leaf).
    Bip44Standard,
    /// `m/44'/60'/{index}'/0/0` — Ledger Live (varies the hardened account level).
    LedgerLive,
    /// A caller template containing the literal `{index}` placeholder.
    Custom(String),
}

impl PathScheme {
    /// The full derivation path for `index` under this scheme.
    pub fn path_for(&self, index: u32) -> Result<String, AccountError> {
        match self {
            Self::Bip44Standard => Ok(format!("m/44'/60'/0'/0/{index}")),
            Self::LedgerLive => Ok(format!("m/44'/60'/{index}'/0/0")),
            Self::Custom(t) if t.contains("{index}") => Ok(t.replace("{index}", &index.to_string())),
            Self::Custom(t) => Err(AccountError::InvalidPath(format!(
                "custom path template missing `{{index}}`: {t}"
            ))),
        }
    }
}

/// BIP-39 entropy strength, in mnemonic words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordCount { W12, W15, W18, W21, W24 }

impl WordCount {
    /// Entropy byte length (128–256 bits).
    pub fn entropy_len(&self) -> usize {
        match self { Self::W12 => 16, Self::W15 => 20, Self::W18 => 24, Self::W21 => 28, Self::W24 => 32 }
    }
    pub fn words(&self) -> usize {
        match self { Self::W12 => 12, Self::W15 => 15, Self::W18 => 18, Self::W21 => 21, Self::W24 => 24 }
    }
}

/// A derived account: its address and how it was derived. Carries no key material —
/// a watch-only account is an `Account` with no signer behind it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub index: u32,
    pub path: String,
    pub address: Address,
    pub label: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AccountError {
    #[error("invalid mnemonic phrase")]
    InvalidPhrase,
    #[error("invalid derivation path: {0}")]
    InvalidPath(String),
    #[error("key derivation failed: {0}")]
    Derivation(String),
    #[error("secure RNG unavailable: {0}")]
    Rng(String),
    #[error("keystore error: {0}")]
    Keystore(String),
    #[error("counterfactual prediction failed: {0}")]
    Predict(String),
    /// A read/RPC failure during account discovery.
    #[error(transparent)]
    Rpc(#[from] crate::core::deps::RpcError),
    #[error(transparent)]
    Read(#[from] crate::core::deps::ReadError),
}
```

Note: `InvalidPhrase` is unit (not carrying the phrase) so an error can never leak seed material.

- [ ] **Step 3: `src/core/accounts/mod.rs`**:

```rust
//! Account management: HD derivation schemes, derived-account records, and
//! counterfactual smart-account address prediction.
mod primitives;
pub use primitives::*;
```

- [ ] **Step 4: Register the module** — add to `src/core/mod.rs`: `pub mod accounts;` (place beside the existing `pub mod wallet;` / `pub mod deps;`). Re-export the public types from the crate root in Task 6.

- [ ] **Step 5: Unit test — path schemes** (in `primitives.rs` under `#[cfg(test)]`). The one regression-worthy logic here is the scheme divergence + custom-template validation:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_schemes_diverge_past_index_zero() {
        // Both schemes agree at 0 and diverge afterwards — the documented footgun.
        assert_eq!(PathScheme::Bip44Standard.path_for(0).unwrap(), "m/44'/60'/0'/0/0");
        assert_eq!(PathScheme::LedgerLive.path_for(0).unwrap(),   "m/44'/60'/0'/0/0");
        assert_eq!(PathScheme::Bip44Standard.path_for(3).unwrap(), "m/44'/60'/0'/0/3");
        assert_eq!(PathScheme::LedgerLive.path_for(3).unwrap(),   "m/44'/60'/3'/0/0");
        assert_ne!(
            PathScheme::Bip44Standard.path_for(3).unwrap(),
            PathScheme::LedgerLive.path_for(3).unwrap()
        );
    }

    #[test]
    fn custom_template_requires_index_placeholder() {
        assert_eq!(PathScheme::Custom("m/44'/60'/0'/0/{index}".into()).path_for(7).unwrap(),
                   "m/44'/60'/0'/0/7");
        assert!(matches!(PathScheme::Custom("m/44'/60'/0'/0/0".into()).path_for(1),
                         Err(AccountError::InvalidPath(_))));
    }
}
```

- [ ] **Step 6: Run the gate.** `cargo fmt --check && cargo clippy --all-targets && cargo test accounts::` — expect the two tests pass, zero clippy warnings. Also `cargo build --no-default-features`.

- [ ] **Step 7: Stop — report gate output, leave uncommitted for review.** On approval:

```bash
git add Cargo.toml Cargo.lock src/core/accounts/ src/core/mod.rs
git commit -m "feat(accounts): core primitives, path schemes, deps"
```

---

### Task 2: AccountManager — generate / restore / derive / sign

**Files:**
- Create: `src/adapters/accounts.rs`
- Modify: `src/adapters/mod.rs`, `src/adapters/signers.rs`

**Interfaces:**
- Consumes: `PathScheme`, `WordCount`, `Account`, `AccountError` (Task 1); `LocalSigner` (existing).
- Produces: `AccountManager` with `generate(WordCount) -> Result<Self,_>`, `from_phrase(&str) -> Result<Self,_>`, `with_passphrase(self, impl Into<String>) -> Self`, `with_scheme(self, PathScheme) -> Self`, `account(&self,u32) -> Result<Account,_>`, `account_at_path(&self,&str) -> Result<Account,_>`, `signer(&self,u32) -> Result<LocalSigner,_>`, `signer_at_path(&self,&str) -> Result<LocalSigner,_>`; and on `LocalSigner`: `from_mnemonic_path(phrase:&str, path:&str, password:Option<&str>) -> Result<Self, SignerError>`.

- [ ] **Step 1: Extend `LocalSigner`** (`src/adapters/signers.rs`) — a path+passphrase mnemonic constructor (reuses alloy `MnemonicBuilder`; key stays inside alloy). Add:

```rust
/// Derive from a BIP-39 mnemonic at an explicit derivation path, with an optional
/// BIP-39 passphrase (the "25th word"). Reuses alloy's `MnemonicBuilder` so the private
/// key is materialised only inside alloy's `PrivateKeySigner`.
pub fn from_mnemonic_path(
    phrase: &str,
    path: &str,
    password: Option<&str>,
) -> Result<Self, SignerError> {
    let mut b = MnemonicBuilder::<English>::default()
        .phrase(phrase)
        .derivation_path(path)
        .map_err(load)?;
    if let Some(pw) = password {
        b = b.password(pw);
    }
    Ok(Self { inner: b.build().map_err(load)? })
}
```

- [ ] **Step 2: `src/adapters/accounts.rs`** — the seed-owning factory. Real code:

```rust
//! `AccountManager` — a seed-owning HD account factory. Holds one BIP-39 mnemonic in
//! zeroizing memory and derives accounts/signers under it; the private key of a derived
//! account is materialised only inside alloy's signer (never exported here).

use crate::adapters::LocalSigner;
use crate::core::accounts::{Account, AccountError, PathScheme, WordCount};
use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use coins_bip39::{English, Entropy, Mnemonic};
use rand::rngs::OsRng;
use rand::RngCore;
use std::collections::HashMap;
use zeroize::Zeroizing;

pub struct AccountManager {
    phrase: Zeroizing<String>,
    passphrase: Option<Zeroizing<String>>,
    scheme: PathScheme,
    labels: HashMap<u32, String>,
}

// The mnemonic must never leak through logs/Debug (it derives every account key).
impl std::fmt::Debug for AccountManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountManager")
            .field("phrase", &"<redacted>")
            .field("passphrase", &self.passphrase.as_ref().map(|_| "<redacted>"))
            .field("scheme", &self.scheme)
            .finish()
    }
}

impl AccountManager {
    /// Generate a fresh mnemonic from the OS CSPRNG (fail-closed: an unavailable RNG is an
    /// error, never a weaker fallback). Retains the phrase, unlike alloy's `build_random`.
    pub fn generate(words: WordCount) -> Result<Self, AccountError> {
        Self::generate_with(words, &mut OsRng)
    }

    // Seam for a deterministic RNG in tests; production always uses OsRng.
    pub(crate) fn generate_with<R: RngCore>(words: WordCount, rng: &mut R) -> Result<Self, AccountError> {
        let mut entropy = Zeroizing::new(vec![0u8; words.entropy_len()]);
        rng.try_fill_bytes(&mut entropy)
            .map_err(|e| AccountError::Rng(e.to_string()))?;
        let ent = Entropy::from_slice(&entropy).map_err(|e| AccountError::Rng(e.to_string()))?;
        let phrase = Zeroizing::new(Mnemonic::<English>::new_from_entropy(ent).to_phrase());
        Ok(Self { phrase, passphrase: None, scheme: PathScheme::Bip44Standard, labels: HashMap::new() })
    }

    /// Restore from an existing phrase; validates the BIP-39 checksum.
    pub fn from_phrase(phrase: &str) -> Result<Self, AccountError> {
        Mnemonic::<English>::new_from_phrase(phrase).map_err(|_| AccountError::InvalidPhrase)?;
        Ok(Self {
            phrase: Zeroizing::new(phrase.to_string()),
            passphrase: None,
            scheme: PathScheme::Bip44Standard,
            labels: HashMap::new(),
        })
    }

    /// Set the BIP-39 passphrase (the "25th word"; changes the whole address set).
    pub fn with_passphrase(mut self, passphrase: impl Into<String>) -> Self {
        self.passphrase = Some(Zeroizing::new(passphrase.into()));
        self
    }
    pub fn with_scheme(mut self, scheme: PathScheme) -> Self {
        self.scheme = scheme;
        self
    }

    fn password(&self) -> Option<&str> {
        self.passphrase.as_deref().map(|s| s.as_str())
    }

    /// Address at a path, derived from the public key only (no full signer built).
    fn address_at_path(&self, path: &str) -> Result<Address, AccountError> {
        let mnemonic = Mnemonic::<English>::new_from_phrase(&self.phrase)
            .map_err(|_| AccountError::InvalidPhrase)?;
        let xpriv = mnemonic
            .derive_key(path, self.password())
            .map_err(|e| AccountError::Derivation(e.to_string()))?;
        Ok(public_key_to_address(xpriv.verify_key().as_ref()))
    }

    pub fn account_at_path(&self, path: &str) -> Result<Account, AccountError> {
        Ok(Account { index: 0, path: path.to_string(), address: self.address_at_path(path)?, label: None })
    }

    pub fn account(&self, index: u32) -> Result<Account, AccountError> {
        let path = self.scheme.path_for(index)?;
        Ok(Account {
            index,
            address: self.address_at_path(&path)?,
            label: self.labels.get(&index).cloned(),
            path,
        })
    }

    pub fn signer_at_path(&self, path: &str) -> Result<LocalSigner, AccountError> {
        LocalSigner::from_mnemonic_path(&self.phrase, path, self.password())
            .map_err(|e| AccountError::Derivation(e.to_string()))
    }

    pub fn signer(&self, index: u32) -> Result<LocalSigner, AccountError> {
        self.signer_at_path(&self.scheme.path_for(index)?)
    }
}
```

- [ ] **Step 3: Register** — add `pub mod accounts;` and `pub use accounts::AccountManager;` to `src/adapters/mod.rs`.

- [ ] **Step 4: Unit tests** (in `accounts.rs`). Regression-worthy: derived addresses match a known vector; `signer(i).address() == account(i).address`; passphrase changes the address; generation yields the right word count; redaction. Use the Foundry test mnemonic (already trusted in `signers.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn derives_known_bip44_addresses_and_signer_matches() {
        let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
        assert_eq!(mgr.account(0).unwrap().address, address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"));
        assert_eq!(mgr.account(1).unwrap().address, address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8"));
        // The public-key address path and the alloy signer path must agree.
        assert_eq!(mgr.signer(1).unwrap().address(), mgr.account(1).unwrap().address);
    }

    #[test]
    fn passphrase_changes_the_address_set() {
        let base = AccountManager::from_phrase(MNEMONIC).unwrap();
        let with = AccountManager::from_phrase(MNEMONIC).unwrap().with_passphrase("trezor");
        assert_ne!(base.account(0).unwrap().address, with.account(0).unwrap().address);
    }

    #[test]
    fn ledger_live_scheme_differs_from_standard() {
        let std = AccountManager::from_phrase(MNEMONIC).unwrap();
        let led = AccountManager::from_phrase(MNEMONIC).unwrap().with_scheme(PathScheme::LedgerLive);
        assert_eq!(std.account(0).unwrap().address, led.account(0).unwrap().address); // agree at 0
        assert_ne!(std.account(1).unwrap().address, led.account(1).unwrap().address); // differ after
    }

    #[test]
    fn generate_yields_requested_word_count_and_valid_checksum() {
        let mgr = AccountManager::generate(WordCount::W24).unwrap();
        // A valid, re-derivable account proves the phrase checksum is good.
        assert!(mgr.account(0).is_ok());
        assert_eq!(mgr.phrase.split(' ').count(), 24);
    }

    #[test]
    fn from_phrase_rejects_bad_checksum_without_leaking() {
        assert!(matches!(AccountManager::from_phrase("not a real mnemonic phrase at all here"),
                         Err(AccountError::InvalidPhrase)));
    }

    #[test]
    fn debug_redacts_the_phrase() {
        let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
        let dbg = format!("{mgr:?}");
        assert!(!dbg.contains("junk"), "phrase leaked in Debug: {dbg}");
    }
}
```

- [ ] **Step 5: Gate.** `cargo fmt --check && cargo clippy --all-targets && cargo test accounts::` (+ `--no-default-features` build). Report output.

- [ ] **Step 6: Stop for review.** On approval:

```bash
git add src/adapters/accounts.rs src/adapters/mod.rs src/adapters/signers.rs
git commit -m "feat(accounts): AccountManager generate/restore/derive/sign"
```

---

### Task 3: Watch-only — account xpub & keyless address derivation

**Files:**
- Modify: `src/adapters/accounts.rs`, `src/core/accounts/primitives.rs`

**Interfaces:**
- Produces: `AccountXpub` (newtype over `coins_bip32::XPub`); `AccountManager::account_xpub(&self, account:u32) -> Result<AccountXpub, AccountError>`; free fn `derive_address(xpub:&AccountXpub, index:u32) -> Result<Address, AccountError>`.

- [ ] **Step 1: `AccountXpub` newtype** (in `primitives.rs`) — a redacting-Debug wrapper so an xpub (public, but fingerprintable) doesn't sprawl through logs:

```rust
/// An account-level BIP-32 extended public key: derive receive addresses without the seed.
pub struct AccountXpub(pub(crate) coins_bip32::xkeys::XPub);

impl std::fmt::Debug for AccountXpub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccountXpub(..)")
    }
}
```

- [ ] **Step 2: `account_xpub` + `derive_address`** (in `accounts.rs`). The account node is hardened, so we neuter *there*; the `0/{index}` tail is non-hardened and derivable from the xpub:

```rust
use crate::core::accounts::AccountXpub;
use coins_bip32::path::DerivationPath;

impl AccountManager {
    /// The account-level extended public key `m/44'/60'/{account}'`. Addresses under it
    /// (`.../0/{index}`) derive without the private key — the watch-only seam.
    pub fn account_xpub(&self, account: u32) -> Result<AccountXpub, AccountError> {
        let path = format!("m/44'/60'/{account}'");
        let mnemonic = Mnemonic::<English>::new_from_phrase(&self.phrase)
            .map_err(|_| AccountError::InvalidPhrase)?;
        let xpriv = mnemonic.derive_key(path.as_str(), self.password())
            .map_err(|e| AccountError::Derivation(e.to_string()))?;
        Ok(AccountXpub(xpriv.verify_key()))
    }
}

/// Derive the receive address at `0/{index}` under an account xpub (BIP-44 standard tail).
pub fn derive_address(xpub: &AccountXpub, index: u32) -> Result<Address, AccountError> {
    let tail: DerivationPath = format!("m/0/{index}").parse()
        .map_err(|e: coins_bip32::Bip32Error| AccountError::InvalidPath(e.to_string()))?;
    let child = xpub.0.derive_path(&tail)
        .map_err(|e| AccountError::Derivation(e.to_string()))?;
    Ok(alloy_signer::utils::public_key_to_address(child.as_ref()))
}
```

(Re-export `derive_address` and `AccountXpub` from `src/adapters/mod.rs` / `src/core/accounts/mod.rs` as appropriate.)

- [ ] **Step 3: Unit test** — watch-only derivation must match full derivation:

```rust
#[test]
fn watch_only_xpub_derives_same_addresses_as_the_seed() {
    let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
    let xpub = mgr.account_xpub(0).unwrap();
    // m/44'/60'/0' + 0/{index} == m/44'/60'/0'/0/{index} == account(index) under Bip44Standard.
    for i in 0..3u32 {
        assert_eq!(derive_address(&xpub, i).unwrap(), mgr.account(i).unwrap().address);
    }
}
```

- [ ] **Step 4: Gate + stop for review.** On approval:

```bash
git add src/adapters/accounts.rs src/core/accounts/primitives.rs src/core/accounts/mod.rs src/adapters/mod.rs
git commit -m "feat(accounts): watch-only account xpub + keyless address derivation"
```

---

### Task 4: predict_address — counterfactual CREATE2 + deploy data

**Files:**
- Modify: `src/core/accounts/primitives.rs`

**Interfaces:**
- Produces: `EntryPointVersion{V0_6,V0_7}`, `DeployData{factory:Address,factory_data:Bytes,entry_point_version:EntryPointVersion}` (+ `init_code()`), `PredictedAccount{address,salt:B256,deploy:Option<DeployData>,is_deployed:Option<bool>}`, `PredictParams{factory,salt:B256,init_code_hash:B256,deploy:Option<DeployData>,entry_point_version}`, `predict_address(&PredictParams)->PredictedAccount`, `predict_address_checked(&dyn ReadClient,&PredictParams)->Result<PredictedAccount,AccountError>`, and helper `safe_salt(initializer:&[u8], salt_nonce:U256)->B256`.

- [ ] **Step 1: Types + pure predictor** (in `primitives.rs`). Reuses `alloy_primitives::Address::create2` — never assemble the `0xff‖…` preimage by hand:

```rust
use alloy_primitives::{Bytes, B256, U256, keccak256};
use crate::core::deps::ReadClient;

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointVersion { V0_6, V0_7 }

/// Deploy data for a counterfactual smart account: exactly what a Phase-5 ERC-4337 deploy
/// and an EIP-6492 signature wrapper need. Canonical form is the v0.7 split; the v0.6
/// `init_code` view is `factory ++ factory_data`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployData {
    pub factory: Address,
    pub factory_data: Bytes,
    pub entry_point_version: EntryPointVersion,
}
impl DeployData {
    /// v0.6 packed `initCode = factory (20 bytes) ++ factory_data`.
    pub fn init_code(&self) -> Bytes {
        let mut v = self.factory.to_vec();
        v.extend_from_slice(&self.factory_data);
        v.into()
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedAccount {
    pub address: Address,
    pub salt: B256,
    pub deploy: Option<DeployData>,
    /// `None` = not checked (pure computation); `Some` from `predict_address_checked`.
    pub is_deployed: Option<bool>,
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct PredictParams {
    pub factory: Address,
    pub salt: B256,
    pub init_code_hash: B256,
    pub deploy: Option<DeployData>,
    pub entry_point_version: EntryPointVersion,
}

/// Predict a smart account's counterfactual CREATE2 address. Pure; no network.
pub fn predict_address(params: &PredictParams) -> PredictedAccount {
    PredictedAccount {
        address: params.factory.create2(params.salt, params.init_code_hash),
        salt: params.salt,
        deploy: params.deploy.clone(),
        is_deployed: None,
    }
}

/// As `predict_address`, plus an on-chain code check via the F1 read port.
pub async fn predict_address_checked(
    read: &dyn ReadClient,
    params: &PredictParams,
) -> Result<PredictedAccount, AccountError> {
    let mut acct = predict_address(params);
    acct.is_deployed = Some(read.is_contract(acct.address).await?);
    Ok(acct)
}

/// Safe's CREATE2 salt: `keccak256(keccak256(initializer) ++ saltNonce)` (double-hashed).
/// The caller supplies the proxy `init_code_hash` (version-specific bytecode) to
/// `predict_address`; this only fixes the salt, the part most often gotten wrong.
pub fn safe_salt(initializer: &[u8], salt_nonce: U256) -> B256 {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(keccak256(initializer).as_slice());
    buf.extend_from_slice(&salt_nonce.to_be_bytes::<32>());
    keccak256(buf)
}
```

- [ ] **Step 2: Unit tests** — a known CREATE2 vector and the Safe double-hash. Use the canonical EIP-1014 example (deployer `0x0000…0000`, salt `0`, init_code `0x00`) whose result is well known, plus assert `safe_salt` is the double (not single) hash:

```rust
#[cfg(test)]
mod predict_tests {
    use super::*;
    use alloy_primitives::{address, b256, bytes};

    #[test]
    fn create2_matches_eip1014_vector() {
        // EIP-1014: address(0), salt 0x00..00, init_code 0x00 -> 0x4D1A2e2bB4F88F0250f26Ffff098B0b30B26Bf38
        let p = PredictParams {
            factory: Address::ZERO,
            salt: B256::ZERO,
            init_code_hash: keccak256(bytes!("0x00")),
            deploy: None,
            entry_point_version: EntryPointVersion::V0_7,
        };
        assert_eq!(predict_address(&p).address, address!("0x4D1A2e2bB4F88F0250f26Ffff098B0b30B26Bf38"));
        assert_eq!(predict_address(&p).is_deployed, None);
    }

    #[test]
    fn safe_salt_is_double_hashed() {
        let initializer = bytes!("0xdeadbeef");
        let nonce = U256::from(1u64);
        let expected = {
            let mut b = Vec::new();
            b.extend_from_slice(keccak256(&initializer).as_slice());
            b.extend_from_slice(&nonce.to_be_bytes::<32>());
            keccak256(b)
        };
        assert_eq!(safe_salt(&initializer, nonce), expected);
        // Must NOT equal the single-hash form — the classic Safe bug.
        assert_ne!(safe_salt(&initializer, nonce), keccak256([&initializer[..], &nonce.to_be_bytes::<32>()].concat()));
    }

    #[test]
    fn deploy_data_init_code_is_factory_then_data() {
        let d = DeployData { factory: address!("0x1111111111111111111111111111111111111111"),
                             factory_data: bytes!("0xabcd"), entry_point_version: EntryPointVersion::V0_6 };
        assert_eq!(d.init_code(), bytes!("0x1111111111111111111111111111111111111111abcd"));
    }
}
```

(Verify the EIP-1014 expected address during implementation; if the canonical constant differs, compute it once with `cast` and pin the real value — never leave it approximate.)

- [ ] **Step 3: Gate + stop for review.** On approval:

```bash
git add src/core/accounts/primitives.rs
git commit -m "feat(accounts): predict_address counterfactual CREATE2 + deploy data"
```

---

### Task 5: Discovery — gap-limit scan over ReadClient/Rpc

**Files:**
- Modify: `src/adapters/accounts.rs`, `src/core/accounts/primitives.rs`

**Interfaces:**
- Consumes: `ReadClient`, `Rpc` ports; `AccountManager` derivation (Task 2).
- Produces: `ChainView{read:Arc<dyn ReadClient>, rpc:Arc<dyn Rpc>}`, `UsedPredicate{NonceOnly,NonceOrBalance}`, `DiscoveryOpts{schemes,gap_limit,max_index,used,start_index}` (+ `Default`), `DiscoveredAccounts{accounts,scanned_to,hit_max_index,partial}`, `AccountManager::discover(&self,&[ChainView],DiscoveryOpts)->Result<DiscoveredAccounts,AccountError>`.

- [ ] **Step 1: Discovery types** (in `primitives.rs`):

```rust
use std::sync::Arc;
use crate::core::deps::Rpc;

/// One chain's read + RPC access. Our ports are per-endpoint, so a chain is a pair.
#[derive(Clone)]
pub struct ChainView {
    pub read: Arc<dyn ReadClient>,
    pub rpc: Arc<dyn Rpc>,
}

/// What marks a derived address "used". `NonceOnly` is one cheap, definitive call for
/// outbound activity; `NonceOrBalance` adds a native-balance read to catch receive-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsedPredicate { NonceOnly, NonceOrBalance }

#[derive(Clone)]
pub struct DiscoveryOpts {
    pub schemes: Vec<PathScheme>,
    pub gap_limit: usize,
    pub max_index: usize,
    pub used: UsedPredicate,
    pub start_index: usize,
}
impl Default for DiscoveryOpts {
    fn default() -> Self {
        Self { schemes: vec![PathScheme::Bip44Standard], gap_limit: 20, max_index: 256,
               used: UsedPredicate::NonceOrBalance, start_index: 0 }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DiscoveredAccounts {
    pub accounts: Vec<Account>,
    pub scanned_to: u32,
    /// Stopped at `max_index` rather than the gap — result is partial.
    pub hit_max_index: bool,
    /// A chain/RPC errored mid-scan (can only hide usage, never invent it).
    pub partial: bool,
}
```

- [ ] **Step 2: `discover`** (in `accounts.rs`). Gap-limit loop per scheme, union across chains, dedup at index 0:

```rust
use crate::core::accounts::{ChainView, DiscoveredAccounts, DiscoveryOpts, UsedPredicate};
use std::collections::BTreeMap;

impl AccountManager {
    /// Scan the seed for used accounts: derive index `i` per scheme, mark used if
    /// `tx_count > 0` (and, for `NonceOrBalance`, `|| native_balance > 0`) on ANY chain,
    /// and stop after `gap_limit` consecutive unused. Always explicit — never on construct.
    pub async fn discover(
        &self,
        chains: &[ChainView],
        opts: DiscoveryOpts,
    ) -> Result<DiscoveredAccounts, AccountError> {
        let mut found: BTreeMap<Address, Account> = BTreeMap::new();
        let mut scanned_to = 0u32;
        let mut hit_max = false;
        let mut partial = false;

        for scheme in &opts.schemes {
            let mut consecutive_unused = 0usize;
            let mut i = opts.start_index;
            loop {
                if i >= opts.max_index { hit_max = true; break; }
                let idx = i as u32;
                let path = scheme.path_for(idx)?;
                let address = self.address_at_path(&path)?;
                scanned_to = scanned_to.max(idx);

                let used = match self.address_used(address, chains, opts.used).await {
                    Ok(u) => u,
                    Err(_) => { partial = true; false } // a chain outage hides, never invents
                };
                if used {
                    consecutive_unused = 0;
                    found.entry(address).or_insert(Account {
                        index: idx, path, address, label: self.labels.get(&idx).cloned(),
                    });
                } else {
                    consecutive_unused += 1;
                    if consecutive_unused >= opts.gap_limit { break; }
                }
                i += 1;
            }
        }
        let mut accounts: Vec<Account> = found.into_values().collect();
        accounts.sort_by_key(|a| a.index);
        Ok(DiscoveredAccounts { accounts, scanned_to, hit_max_index: hit_max, partial })
    }

    async fn address_used(&self, addr: Address, chains: &[ChainView], pred: UsedPredicate)
        -> Result<bool, AccountError> {
        for c in chains {
            if c.rpc.tx_count(addr).await? > 0 { return Ok(true); }
            if pred == UsedPredicate::NonceOrBalance && !c.read.native_balance(addr).await?.is_zero() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
```

- [ ] **Step 3: Integration test** (`tests/accounts.rs`, hermetic anvil). Fund/txn a couple of indices, assert the scan finds exactly those and stops on the gap. Reuse the existing anvil harness (`tests/support`). Sketch:

```rust
// Using tests/support: spin anvil, wrap its endpoint in a ChainView (RpcTransport + RpcReadClient).
// Send 1 tx from account(0) (nonce>0) and fund account(2) (balance>0, nonce 0); leave 1 unused.
// discover(gap_limit=3) must return indices [0,2], scanned_to>=2, hit_max_index=false, partial=false.
#[tokio::test]
async fn discovers_used_accounts_and_stops_on_gap() { /* real harness wiring here */ }
```

(Write the concrete harness wiring against `tests/support/mod.rs` helpers; assert exact indices `[0,2]` and the `NonceOnly` vs `NonceOrBalance` difference — `NonceOnly` must miss the receive-only index 2.)

- [ ] **Step 4: Gate + stop for review.** On approval:

```bash
git add src/adapters/accounts.rs src/core/accounts/primitives.rs tests/accounts.rs
git commit -m "feat(accounts): BIP-44 discovery with gap-limit, union, partial flags"
```

---

### Task 6: Keystore, labels, error wiring & docs

**Files:**
- Modify: `src/adapters/signers.rs`, `src/adapters/accounts.rs`, `src/error.rs`, `src/lib.rs`, `README.md`, `CHANGELOG.md`

**Interfaces:**
- Produces: `LocalSigner::export_keystore(&self, dir:&Path, password:&str) -> Result<PathBuf, SignerError>`, `LocalSigner::import_keystore(path:&Path, password:&str) -> Result<Self, SignerError>`; `AccountManager::set_label`, `account_by_label`; `WalletKitError::Account(AccountError)`.

- [ ] **Step 1: Keystore on `LocalSigner`** (`signers.rs`) — reuse alloy's Web3-Secret-Storage:

```rust
use std::path::{Path, PathBuf};
use rand::rngs::OsRng;
use zeroize::Zeroizing;

impl LocalSigner {
    /// Encrypt this account's key to a Web3-Secret-Storage/EIP-2335 keystore JSON
    /// (scrypt + AES-128-CTR — the MetaMask/Geth/Foundry format). Returns the file path.
    pub fn export_keystore(&self, dir: &Path, password: &str) -> Result<PathBuf, SignerError> {
        let pk = Zeroizing::new(self.inner.to_bytes()); // B256; zeroized after use
        let (_, name) = alloy_signer_local::PrivateKeySigner::encrypt_keystore(
            dir, &mut OsRng, pk.as_slice(), password, None,
        ).map_err(load)?;
        Ok(dir.join(name))
    }

    /// Decrypt a keystore JSON into a signer.
    pub fn import_keystore(path: &Path, password: &str) -> Result<Self, SignerError> {
        Ok(Self { inner: alloy_signer_local::PrivateKeySigner::decrypt_keystore(path, password).map_err(load)? })
    }
}
```

- [ ] **Step 2: Labels on `AccountManager`** (`accounts.rs`):

```rust
impl AccountManager {
    /// Attach a human-readable label to a derived account index.
    pub fn set_label(&mut self, index: u32, name: impl Into<String>) {
        self.labels.insert(index, name.into());
    }
    /// The first account whose label matches `name`, if any.
    pub fn account_by_label(&self, name: &str) -> Option<Account> {
        self.labels.iter().find(|(_, v)| v.as_str() == name)
            .and_then(|(&i, _)| self.account(i).ok())
    }
}
```

- [ ] **Step 3: Error wiring** (`error.rs`) — add the variant, `From`, and `kind()` arm:

```rust
// in enum WalletKitError:
#[error(transparent)]
Account(#[from] crate::core::accounts::AccountError),
```
In `kind()`: classify `Account(_)` as `Terminal`, except `AccountError::Rpc`/`Read` which delegate to the inner error's classification (bad phrase/path/keystore are terminal; a discovery RPC failure is retryable). Mirror the existing `Read`/`Ens` arms.

- [ ] **Step 4: Crate surface + docs.** Re-export from `src/lib.rs`: `AccountManager`, `Account`, `PathScheme`, `WordCount`, `AccountXpub`, `derive_address`, `predict_address`, `predict_address_checked`, `PredictedAccount`, `DeployData`, `DiscoveryOpts`, `DiscoveredAccounts`, `ChainView`, `UsedPredicate`. Add a "Accounts (HD, discovery, prediction)" section to `README.md` with a short example (`generate` → `signer(0)` → `Wallet::builder`).

- [ ] **Step 5: CHANGELOG.** Add under `[Unreleased]` / `Added`:

```markdown
- **HD account management** (F2): `AccountManager` — BIP-39 seed generate/restore (fail-closed
  CSPRNG, zeroized), BIP-44/Ledger-Live derivation, watch-only account xpubs, `predict_address`
  (counterfactual CREATE2 + EIP-4337/6492 deploy data), gap-limit account discovery, and
  encrypted keystore import/export.
```

- [ ] **Step 6: Keystore round-trip test** (`tests/accounts.rs` or `accounts.rs` unit with a tempdir):

```rust
#[test]
fn keystore_round_trip_recovers_the_address() {
    let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
    let signer = mgr.signer(0).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = signer.export_keystore(dir.path(), "pw").unwrap();
    let restored = LocalSigner::import_keystore(&path, "pw").unwrap();
    assert_eq!(restored.address(), signer.address());
    assert!(LocalSigner::import_keystore(&path, "wrong").is_err());
}
```

(`tempfile` is already a dev-dependency in this repo — confirm; if not, add it under `[dev-dependencies]`.)

- [ ] **Step 7: Full gate** — `cargo fmt --check && cargo clippy --all-targets && cargo test` and `cargo build --no-default-features`. Report real output.

- [ ] **Step 8: Stop for review.** On approval:

```bash
git add -A
git commit -m "feat(accounts): keystore import/export, labels, error wiring, docs"
```

Then run the phase-close refactor/review pass over the whole slice, open the PR (`feat/account-manager` → `main`), and merge on green (matching the F1 convention).

## Self-Review

**Spec coverage:** ✅ seed generate/restore+passphrase (T2), PathScheme incl. Ledger Live (T1/T2), account(index)/account_at_path + signer minting (T2), watch-only xpub (T3), predict_address + DeployData/EIP-6492 fields + Safe salt (T4), discovery gap-limit/union/partial (T5), keystore import/export + labels + WalletKitError + docs/changelog (T6). Deferred items (session keys, multisig, 7702, social recovery, mlock, raw-mnemonic reveal) intentionally absent.

**Placeholder scan:** The only deferred concretization is the Task-5 anvil harness wiring and the Task-4 EIP-1014 constant — both flagged to be pinned to real values during implementation (compute with `cast`, never approximate), consistent with the F1 exact-value testing discipline.

**Type consistency:** `AccountError`, `PathScheme`, `Account`, `AccountXpub`, `DeployData`, `PredictedAccount`, `PredictParams`, `ChainView`, `DiscoveryOpts`, `DiscoveredAccounts`, `UsedPredicate` are defined once (T1/T3/T4/T5) and referenced consistently; `address_at_path`/`password` helpers (T2) are reused by T3/T5; `LocalSigner::from_mnemonic_path` (T2) underpins `signer*`.
