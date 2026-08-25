# F2 — AccountManager: design

**Sub-project:** F2 (of Phase 1 DX seams F1→F2→F3). **Date:** 2026-08-24. **Status:** design approved; plan pending.

Depends on F1 (`ReadClient`, `Rpc` ports). Builds the SPEC §7 reserved seam **`AccountManager` (HD discovery + `predict_address`)**.

## 1. Goal & scope

Give walletkit a proper HD account-management layer: **generate a BIP-39 seed phrase, derive multiple accounts under one seed, discover which accounts a seed actually used, and predict counterfactual smart-account addresses.** Today HD derivation exists only as `LocalSigner::from_mnemonic(phrase, index)` (one index at a time, path fixed to `m/44'/60'/0'/0/{index}`); alloy's own `MnemonicBuilder::build_random()` throws the mnemonic away, so there is no "generate a seed, keep it, derive N accounts" primitive. `AccountManager` fills that gap.

**In scope (Full F2):** seed generate/restore (+ BIP-39 passphrase), `PathScheme` (MetaMask vs Ledger Live), `account(index)`/`account_at_path` derivation, signer minting, watch-only (xpub), account labels, **BIP-44 account discovery** (gap-limit scan), **`predict_address`** (counterfactual CREATE2 + reserved deploy data), and **keystore import/export** (EIP-2335/Web3-Secret-Storage via alloy).

**Deferred (later phases, lean on the existing policy engine — not new machinery):** session keys, multi-owner/threshold + owner rotation, EIP-7702 sub-accounts + spend permissions, social recovery. **Backup posture:** encrypted keystore export only — **no raw-mnemonic reveal API**. **Out of scope:** address book (an app concern). `mlock`/core-dump hardening is a deferred optional feature.

## 2. Model & naming

Adopt the industry-universal two-level HD model (Turnkey/Fireblocks/Circle/Safe), mapped to names that don't collide with our existing `Wallet` (the per-account runtime):

- **`AccountManager`** — the seed-owning HD container/factory. Holds one BIP-39 mnemonic; derives accounts; mints signers.
- **`Account`** — a derived entry `{ index, path, address, label }`. Carries **no key material**; a watch-only account is just an `Account` with no signer behind it.
- **`Signer`** (existing port) stays "the signing capability." `AccountManager::signer(i)` returns a `LocalSigner` that plugs into today's `Wallet::builder(rpc, signer, policy)` — **no facade change required.**

(We deliberately diverge from Privy's naming, where "wallet" means a single address — it conflicts with BIP-44's "account".)

## 3. Architecture & hexagonal placement

`AccountManager` is a **concrete adapter** (like `LocalSigner`), **not** a new port: there is one implementation, so a `SeedVault`/`KeyBackend` port is YAGNI until a second custody backend exists. An internal `SeedSource`-style seam keeps that promotion possible later without touching the `Signer` contract.

- **`core/accounts/primitives.rs`** (zero-I/O, pure): `PathScheme`, `WordCount`, `Account`, `PredictedAccount`, `DeployData`, `EntryPointVersion`, `AccountError`, discovery result types, `predict_address()` (CREATE2), and path helpers.
- **`adapters/accounts.rs`** (seed-owning, key material): `AccountManager` — generation, derivation, signer minting, labels, watch-only xpub, keystore import/export, discovery orchestration over the `ReadClient`/`Rpc` ports.

`AccountManager` **mints** `LocalSigner`s but the private key still never leaves alloy — the F1 "signature-only, no export" invariant holds because the manager only *constructs* signers (mirroring alloy's own `MnemonicBuilder → PrivateKeySigner`).

## 4. `AccountManager` API

```rust
#[non_exhaustive]
pub enum PathScheme {
    Bip44Standard,   // m/44'/60'/0'/0/{index}   — MetaMask/viem/ethers/alloy default
    LedgerLive,      // m/44'/60'/{index}'/0/0
    Custom(String),  // template containing "{index}"
}
pub enum WordCount { W12, W15, W18, W21, W24 }   // maps to coins-bip39's valid entropy set

#[non_exhaustive]
pub struct Account { pub index: u32, pub path: String, pub address: Address, pub label: Option<String> }

pub struct AccountManager { /* Zeroizing mnemonic, Option<passphrase>, PathScheme, labels map */ }

impl AccountManager {
    // generate / restore
    pub fn generate(words: WordCount) -> Result<Self, AccountError>;        // fail-closed OsRng
    pub fn from_phrase(phrase: &str) -> Result<Self, AccountError>;         // BIP-39 checksum-validated
    pub fn with_passphrase(self, p: impl Into<String>) -> Self;            // BIP-39 "25th word"
    pub fn with_scheme(self, s: PathScheme) -> Self;

    // derive
    pub fn account(&self, index: u32) -> Result<Account, AccountError>;
    pub fn account_at_path(&self, path: &str) -> Result<Account, AccountError>;
    pub fn signer(&self, index: u32) -> Result<LocalSigner, AccountError>; // → Wallet::builder
    pub fn signer_at_path(&self, path: &str) -> Result<LocalSigner, AccountError>;

    // labels (get-or-create is inherent — derivation is deterministic)
    pub fn set_label(&mut self, index: u32, name: impl Into<String>);
    pub fn account_by_label(&self, name: &str) -> Option<Account>;

    // watch-only
    pub fn account_xpub(&self, account: u32) -> Result<AccountXpub, AccountError>; // coins-bip32 XPub
}

/// Keyless: derive receive addresses from a shared account-level xpub, no seed present.
pub fn derive_address(xpub: &AccountXpub, index: u32) -> Result<Address, AccountError>;
```

**Path-scheme footgun (documented invariant):** `Bip44Standard` and `LedgerLive` produce entirely different address sets from the same seed (they agree only at index 0). Default is `Bip44Standard`; `index` documents *which slot it varies*. Hardened derivation is impossible from an xpub, so `account_xpub` neuters at the **account** node (`m/44'/60'/{account}'`), whose non-hardened tail is then derivable watch-only.

## 5. Discovery

```rust
pub struct DiscoveryOpts {
    pub schemes: Vec<PathScheme>,  // default [Bip44Standard]; add LedgerLive to cover hardware-wallet users
    pub gap_limit: usize,          // default 20 (BIP-44)
    pub max_index: usize,          // hard bound, default 256
    pub used: UsedPredicate,       // default NonceOrBalance
    pub start_index: usize,        // default 0 (resume support)
}
pub enum UsedPredicate { NonceOnly, NonceOrBalance }   // reuses Rpc::tx_count + ReadClient::native_balance

#[non_exhaustive]
pub struct DiscoveredAccounts {
    pub accounts: Vec<Account>,
    pub scanned_to: u32,
    pub hit_max_index: bool,  // true => stopped at the bound, not the gap (partial!)
    pub partial: bool,        // true => a chain/RPC errored mid-scan
}

impl AccountManager {
    pub async fn discover(&self, chains: &[ChainView], opts: DiscoveryOpts)
        -> Result<DiscoveredAccounts, AccountError>;
}
```

- **Algorithm:** for `i` in `start_index..max_index`, derive the address per scheme; `used(i)` = `tx_count > 0` (and, for `NonceOrBalance`, `|| native_balance > 0`) on **any** chain (union). Stop after `gap_limit` consecutive unused; a used index resets the run. Run once per scheme; union and dedup at index 0.
- **`ChainView`** bundles one chain's `ReadClient` + `Rpc` (our ports are per-endpoint). Common case = pass one; multi-chain unions "used on chain A but empty on B."
- **Always explicit** — never scan on construction. Auto-scan leaks the full address set to the RPC provider, which is exactly why MetaMask keeps it opt-in.
- **Honesty:** `hit_max_index`/`partial` distinguish "complete (hit the gap)" from "truncated"; a chain outage can only hide usage (false-negative), never invent it.
- **Known residual gap (documented):** neither nonce nor native balance catches an address whose only history is receiving an ERC-20. `NonceOrBalance` is the default; ERC-20/indexer completeness is a caller extension, not F2.

## 6. `predict_address` (keyless, pure)

```rust
#[non_exhaustive] pub enum EntryPointVersion { V0_6, V0_7 }

#[non_exhaustive]
pub struct DeployData { pub factory: Address, pub factory_data: Bytes, pub entry_point_version: EntryPointVersion }
impl DeployData { pub fn init_code(&self) -> Bytes; }   // v0.6 packed view = factory ++ factory_data

#[non_exhaustive]
pub struct PredictedAccount {
    pub address: Address,
    pub salt: B256,
    pub deploy: Option<DeployData>,
    pub is_deployed: Option<bool>,   // None = not checked (pure compute)
}

#[non_exhaustive]
pub struct PredictParams { pub factory: Address, pub salt: B256, pub init_code_hash: B256,
                           pub deploy: Option<DeployData>, pub entry_point_version: EntryPointVersion }

pub fn predict_address(params: &PredictParams) -> PredictedAccount;                 // pure Address::create2
pub async fn predict_address_checked(read: &dyn ReadClient, params: &PredictParams) // + is_contract
    -> Result<PredictedAccount, AccountError>;
```

- All counterfactual prediction reduces to CREATE2: `keccak256(0xff ‖ factory ‖ salt ‖ keccak256(init_code))[12:]`, computed via `alloy_primitives::Address::create2` — reuse; never assemble the preimage by hand.
- Ship a **Safe salt helper**: salt is **double-hashed** `keccak256(keccak256(initializer) ‖ saltNonce)`, init code is `SafeProxy.creationCode ‖ pad32(singleton)` (Safe is the most common counterfactual case; getting the double-hash wrong yields a plausible-but-wrong address).
- `(factory, factory_data)` is exactly what a Phase-5 ERC-4337 deploy **and** an EIP-6492 signature wrapper need. `#[non_exhaustive]` + `Option` fields let Phase 5 add bundler submission, `getSenderAddress` fallback, 6492 wrapping, and the 7702 `0x7702` factory sentinel **without a breaking change**. **No bundler / no 6492 machinery in F2.**

## 7. Security & dependencies

**Mandatory:**
- **Fail-closed CSPRNG:** entropy from `OsRng`/`getrandom`; on unavailability return `Err` — never degrade to a weaker source.
- **Zeroization:** `zeroize::Zeroizing` on mnemonic/seed/xpriv. `coins-bip39::Mnemonic` derives `Clone`+`Debug` and `to_seed` returns a bare `[u8;64]` — walletkit **wraps** it, never `.clone()`s it, and drops it fast.
- **Redaction:** every secret-bearing type has a redacting `Debug` (`Mnemonic(REDACTED)`), matching the existing signing redaction discipline; a redaction test guards it.
- **Keystore via alloy:** encrypted export/import uses alloy's Web3-Secret-Storage/EIP-2335 (`encrypt_keystore`/`decrypt_keystore`, scrypt+AES-128-CTR, same format as MetaMask/Geth/Foundry) — reuse, don't hand-roll. Keystore password handled as a secret and zeroized.
- **No raw-mnemonic reveal API.**

**Deferred/optional:** `mlock`/core-dump hardening behind a `locked-memory` feature (best-effort, non-portable) — not in F2.

**New dependencies:** `coins-bip39` + `coins-bip32` (pinned to alloy-signer-local's versions to avoid duplicates), `rand` (for `OsRng`) as a direct dep. **No `secrecy`** — `zeroize` (already a dependency) + a small redacting newtype suffices.

## 8. Wiring, errors, testing

- **Wiring:** zero facade change — `let s = mgr.signer(0)?; Wallet::builder(rpc, Arc::new(s), policy).build();`
- **Errors:** one public `WalletKitError::Account(AccountError)` + `From` + `kind()` classification (predominantly `Terminal`; bad phrase/path/keystore-password are terminal). `AccountError` variants: `InvalidPhrase`, `InvalidPath`, `Derivation`, `Keystore`, `Rng`, `Read`/`Rpc` (discovery), `Predict`.
- **Tests — every test earns its place:**
  - Derivation vectors: the Foundry `test … junk` mnemonic → its known addresses (we already assert these in `signers.rs`); `account(0)`/`account(1)` match.
  - `PathScheme` divergence: standard vs Ledger Live agree at index 0, differ at i≥1.
  - Discovery (hermetic anvil): fund/txn a few indices, assert gap-limit stop, union across chains, `partial`/`hit_max_index` flags, and the ERC-20-only false-negative is documented (not asserted as found).
  - Keystore encrypt→decrypt round-trip recovers the address.
  - `predict_address` against a known CREATE2 vector + a known Safe address vector.
  - Fail-closed generation path; redaction test that no secret type leaks via `Debug`.
  - No tests for struct init / serde / config.

## 9. Task breakdown (each task = one complete component)

1. **Core primitives & paths** — `core/accounts/primitives.rs`: `PathScheme` (+ path builder), `WordCount`, `Account`, `AccountError`; deps added (`coins-bip39`, `coins-bip32`, `rand`).
2. **AccountManager: generate/restore/derive/sign** — seed generation (fail-closed OsRng), `from_phrase`, passphrase, `account`/`account_at_path`, `signer`/`signer_at_path`; zeroization + redaction.
3. **Watch-only** — `account_xpub` + keyless `derive_address`.
4. **predict_address** — `DeployData`/`PredictedAccount`/`EntryPointVersion` + pure `predict_address` + `predict_address_checked` + Safe salt helper.
5. **Discovery** — `DiscoveryOpts`/`UsedPredicate`/`DiscoveredAccounts` + `discover` over `ChainView` (`ReadClient`/`Rpc`), gap-limit/union/partial.
6. **Keystore + labels + wiring** — keystore import/export (alloy), labels/get-or-create, `WalletKitError::Account`, README/crate-doc, `CHANGELOG.md` `[Unreleased]`.

## 10. Research provenance

Five-agent industry survey (2026-08-24): HD library APIs (viem/ethers/alloy-signer-local/coins-bip39/eth-account), BIP-44 discovery & gap-limit, wallet-infra provider account APIs (Turnkey/Privy/Coinbase CDP/thirdweb/ZeroDev/Alchemy/Safe/Circle/Fireblocks), counterfactual prediction + EIP-6492/4337, and Rust seed-security (zeroize/secrecy/getrandom/rand + EIP-2335). Key primary sources: [BIP-44](https://github.com/bitcoin/bips/blob/master/bip-0044.mediawiki), [EIP-6492](https://eips.ethereum.org/EIPS/eip-6492), [EIP-4337](https://eips.ethereum.org/EIPS/eip-4337), [EIP-2335](https://eips.ethereum.org/EIPS/eip-2335), [alloy-signer-local](https://docs.rs/alloy-signer-local/latest/alloy_signer_local/), [coins-bip39](https://docs.rs/coins-bip39/latest/coins_bip39/), [viem accounts](https://viem.sh/docs/accounts/local/mnemonicToAccount), [ledger-live-common derivation](https://github.com/LedgerHQ/ledger-live-common/blob/develop/docs/derivation.md), [Safe protocol-kit](https://docs.safe.global/reference-sdk-protocol-kit/initialization/init).
