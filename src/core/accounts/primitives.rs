//! Pure account-management domain types: HD path schemes, word counts, derived-account
//! records, and watch-only extended public keys. Zero I/O. `AccountError` grows one variant
//! per consumer as the slice fills in.

use alloy_primitives::Address;
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
}
