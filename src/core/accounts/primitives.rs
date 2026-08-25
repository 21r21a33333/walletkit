//! Pure account-management domain types: HD path schemes, word counts, derived-account
//! records, and watch-only extended public keys. Zero I/O. `AccountError` grows one variant
//! per consumer as the slice fills in.

use crate::core::deps::ReadClient;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_signer::utils::public_key_to_address;
use coins_bip32::prelude::Parent;
use coins_bip32::xkeys::XPub;

/// Which BIP-44 slot the `index` shortcut varies. The two standard schemes agree only at
/// index 0 and produce entirely different address sets otherwise — a notorious interop footgun,
/// so `index` always documents *which* slot it moves.
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
    /// The full BIP-32 derivation path for `index` under this scheme.
    pub fn path_for(&self, index: u32) -> Result<String, AccountError> {
        match self {
            Self::Bip44Standard => Ok(format!("m/44'/60'/0'/0/{index}")),
            Self::LedgerLive => Ok(format!("m/44'/60'/{index}'/0/0")),
            Self::Custom(t) if t.contains("{index}") => {
                Ok(t.replace("{index}", &index.to_string()))
            }
            Self::Custom(t) => Err(AccountError::InvalidPath(format!(
                "custom path template missing `{{index}}`: {t}"
            ))),
        }
    }
}

/// BIP-39 entropy strength, expressed in mnemonic words (128–256 bits of entropy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordCount {
    W12,
    W15,
    W18,
    W21,
    W24,
}

impl WordCount {
    /// Entropy byte length backing this word count.
    pub fn entropy_len(&self) -> usize {
        match self {
            Self::W12 => 16,
            Self::W15 => 20,
            Self::W18 => 24,
            Self::W21 => 28,
            Self::W24 => 32,
        }
    }

    /// The number of words in the mnemonic.
    pub fn words(&self) -> usize {
        match self {
            Self::W12 => 12,
            Self::W15 => 15,
            Self::W18 => 18,
            Self::W21 => 21,
            Self::W24 => 24,
        }
    }
}

/// A derived account: its address and how it was derived. Carries no key material — a
/// watch-only account is just an `Account` with no signer behind it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub index: u32,
    pub path: String,
    pub address: Address,
    pub label: Option<String>,
}

/// An account-level BIP-32 extended public key: derive receive addresses without the seed
/// (the watch-only seam). Neutered at the hardened account node `m/44'/60'/{account}'`, so
/// its non-hardened `0/{index}` tail is derivable from the public key alone.
pub struct AccountXpub(pub(crate) XPub);

// An xpub is public but fingerprintable; keep it out of logs by default.
impl std::fmt::Debug for AccountXpub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccountXpub(..)")
    }
}

/// Derive the receive address at `0/{index}` under an account xpub (the BIP-44 standard
/// change/index tail). Keyless and pure — no seed, no I/O.
pub fn derive_address(xpub: &AccountXpub, index: u32) -> Result<Address, AccountError> {
    let child = xpub
        .0
        .derive_path([0u32, index].as_slice())
        .map_err(|e| AccountError::Derivation(e.to_string()))?;
    Ok(public_key_to_address(child.as_ref()))
}

/// The ERC-4337 EntryPoint version a factory targets. Governs how deploy data is expressed:
/// a single packed `initCode` (v0.6) or split `factory` + `factoryData` (v0.7).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointVersion {
    V0_6,
    V0_7,
}

/// Deploy data for a counterfactual smart account: exactly what a later ERC-4337 deploy and
/// an EIP-6492 signature wrapper need. Canonical form is the v0.7 split; the v0.6 `initCode`
/// view is `factory ++ factory_data`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployData {
    pub factory: Address,
    pub factory_data: Bytes,
    pub entry_point_version: EntryPointVersion,
}

impl DeployData {
    /// The v0.6 packed `initCode = factory (20 bytes) ++ factory_data`.
    pub fn init_code(&self) -> Bytes {
        let mut v = self.factory.to_vec();
        v.extend_from_slice(&self.factory_data);
        v.into()
    }
}

/// A predicted counterfactual smart-account address plus the data needed to use it before it
/// exists on-chain. `#[non_exhaustive]`/`Option` fields leave room for later ERC-4337/6492
/// work without a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedAccount {
    /// The CREATE2 address — valid whether or not code exists there yet.
    pub address: Address,
    pub salt: B256,
    pub deploy: Option<DeployData>,
    /// `None` = not checked (pure computation); `Some` only from `predict_address_checked`.
    pub is_deployed: Option<bool>,
}

/// Inputs to a CREATE2 prediction. The caller supplies the factory, the (post-scheme) salt,
/// and the init-code hash; `deploy` optionally carries the factory/factoryData to thread
/// through to a later deploy or 6492 wrapper.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct PredictParams {
    pub factory: Address,
    pub salt: B256,
    pub init_code_hash: B256,
    pub deploy: Option<DeployData>,
}

/// Predict a smart account's counterfactual CREATE2 address:
/// `keccak256(0xff ‖ factory ‖ salt ‖ init_code_hash)[12:]`. Pure — no network. Reuses
/// alloy's `Address::create2`; never assemble the preimage by hand.
pub fn predict_address(params: &PredictParams) -> PredictedAccount {
    PredictedAccount {
        address: params.factory.create2(params.salt, params.init_code_hash),
        salt: params.salt,
        deploy: params.deploy.clone(),
        is_deployed: None,
    }
}

/// As [`predict_address`], plus an on-chain code check via the read port (sets `is_deployed`).
pub async fn predict_address_checked(
    read: &dyn ReadClient,
    params: &PredictParams,
) -> Result<PredictedAccount, AccountError> {
    let mut acct = predict_address(params);
    acct.is_deployed = Some(read.is_contract(acct.address).await?);
    Ok(acct)
}

/// Safe's CREATE2 salt: `keccak256(keccak256(initializer) ‖ saltNonce)` — **double-hashed**.
/// Getting this wrong (single-hashing) yields a plausible-but-wrong address; the caller pairs
/// it with the proxy `init_code_hash` for [`predict_address`].
pub fn safe_salt(initializer: &[u8], salt_nonce: U256) -> B256 {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(keccak256(initializer).as_slice());
    buf.extend_from_slice(&salt_nonce.to_be_bytes::<32>());
    keccak256(buf)
}

/// What marks a derived address "used". `NonceOnly` counts outbound activity (nonce > 0);
/// `NonceOrBalance` also counts a non-zero native balance to catch receive-only addresses
/// (misses ERC-20-only recipients — a documented residual gap). Both signals come from one
/// batched [`account_activity`](crate::core::deps::Rpc::account_activity) call per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsedPredicate {
    NonceOnly,
    NonceOrBalance,
}

/// Options for [`AccountManager::discover`](crate::adapters::AccountManager::discover).
#[derive(Clone)]
pub struct DiscoveryOpts {
    /// Path schemes to enumerate; results are unioned (default `[Bip44Standard]`).
    pub schemes: Vec<PathScheme>,
    /// Stop after this many consecutive unused indices (BIP-44 default 20).
    pub gap_limit: usize,
    /// Hard scan bound; hitting it marks the result partial.
    pub max_index: usize,
    pub used: UsedPredicate,
    /// First index to probe (for resuming a scan).
    pub start_index: usize,
}

impl Default for DiscoveryOpts {
    fn default() -> Self {
        Self {
            schemes: vec![PathScheme::Bip44Standard],
            gap_limit: 20,
            max_index: 256,
            used: UsedPredicate::NonceOrBalance,
            start_index: 0,
        }
    }
}

/// The outcome of a discovery scan.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct DiscoveredAccounts {
    /// Used accounts, index-ordered and deduplicated across schemes.
    pub accounts: Vec<Account>,
    /// The highest index probed.
    pub scanned_to: u32,
    /// Stopped at `max_index` rather than the gap — the result is partial.
    pub hit_max_index: bool,
    /// A chain/RPC errored mid-scan; usage can only be hidden, never invented, so results
    /// are a lower bound.
    pub partial: bool,
}

/// Account-management failures. `#[non_exhaustive]` and grown per consumer; the public
/// boundary maps this into `WalletKitError`. An error never carries seed material.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AccountError {
    #[error("invalid derivation path: {0}")]
    InvalidPath(String),
    /// A phrase failed BIP-39 validation (checksum/word count). Never carries the phrase.
    #[error("invalid mnemonic phrase")]
    InvalidPhrase,
    #[error("key derivation failed: {0}")]
    Derivation(String),
    #[error("secure RNG unavailable: {0}")]
    Rng(String),
    /// A read failed during an on-chain check (e.g. `predict_address_checked`).
    #[error(transparent)]
    Read(#[from] crate::core::deps::ReadError),
    /// An RPC failed during account discovery.
    #[error(transparent)]
    Rpc(#[from] crate::core::deps::RpcError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_schemes_diverge_past_index_zero() {
        // Both schemes agree at 0 and diverge afterwards — the documented footgun.
        assert_eq!(
            PathScheme::Bip44Standard.path_for(0).unwrap(),
            "m/44'/60'/0'/0/0"
        );
        assert_eq!(
            PathScheme::LedgerLive.path_for(0).unwrap(),
            "m/44'/60'/0'/0/0"
        );
        assert_eq!(
            PathScheme::Bip44Standard.path_for(3).unwrap(),
            "m/44'/60'/0'/0/3"
        );
        assert_eq!(
            PathScheme::LedgerLive.path_for(3).unwrap(),
            "m/44'/60'/3'/0/0"
        );
        assert_ne!(
            PathScheme::Bip44Standard.path_for(3).unwrap(),
            PathScheme::LedgerLive.path_for(3).unwrap()
        );
    }

    #[test]
    fn custom_template_requires_index_placeholder() {
        assert_eq!(
            PathScheme::Custom("m/44'/60'/0'/0/{index}".into())
                .path_for(7)
                .unwrap(),
            "m/44'/60'/0'/0/7"
        );
        assert!(matches!(
            PathScheme::Custom("m/44'/60'/0'/0/0".into()).path_for(1),
            Err(AccountError::InvalidPath(_))
        ));
    }

    #[test]
    fn create2_matches_eip1014_vector() {
        // EIP-1014 Example 0: deployer 0x00.., salt 0x00.., init_code 0x00.
        // Address verified with `cast keccak` (note the EIP-55 checksum casing).
        let p = PredictParams {
            factory: Address::ZERO,
            salt: B256::ZERO,
            init_code_hash: keccak256([0x00]),
            deploy: None,
        };
        let out = predict_address(&p);
        assert_eq!(
            out.address,
            alloy_primitives::address!("0x4D1A2e2bB4F88F0250f26Ffff098B0b30B26BF38")
        );
        assert_eq!(out.is_deployed, None); // pure compute leaves it unchecked
        assert_eq!(out.salt, B256::ZERO);
    }

    #[test]
    fn safe_salt_is_double_hashed_not_single() {
        let initializer = alloy_primitives::bytes!("0xdeadbeef");
        let nonce = U256::from(1u64);
        let expected = {
            let mut b = Vec::new();
            b.extend_from_slice(keccak256(&initializer).as_slice());
            b.extend_from_slice(&nonce.to_be_bytes::<32>());
            keccak256(b)
        };
        assert_eq!(safe_salt(&initializer, nonce), expected);
        // Must NOT equal the single-hash form — the classic Safe address bug.
        let single = keccak256([&initializer[..], &nonce.to_be_bytes::<32>()[..]].concat());
        assert_ne!(safe_salt(&initializer, nonce), single);
    }

    #[test]
    fn deploy_data_init_code_is_factory_then_data() {
        let d = DeployData {
            factory: alloy_primitives::address!("0x1111111111111111111111111111111111111111"),
            factory_data: alloy_primitives::bytes!("0xabcd"),
            entry_point_version: EntryPointVersion::V0_6,
        };
        assert_eq!(
            d.init_code(),
            alloy_primitives::bytes!("0x1111111111111111111111111111111111111111abcd")
        );
    }
}
