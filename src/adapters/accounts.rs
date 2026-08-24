//! `AccountManager` — a seed-owning HD account factory. Holds one BIP-39 mnemonic in
//! zeroizing memory and derives accounts/signers under it. A derived account's private key
//! is materialized only inside alloy's signer (never exported here), so the F1
//! signature-only invariant holds; this type only *constructs* signers.

use crate::adapters::LocalSigner;
use crate::core::accounts::{Account, AccountError, PathScheme, WordCount};
use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use coins_bip39::{English, Entropy, Mnemonic};
use rand::RngCore;
use rand::rngs::OsRng;
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Owns a BIP-39 mnemonic (+ optional passphrase) and derives accounts under one seed.
pub struct AccountManager {
    phrase: Zeroizing<String>,
    passphrase: Option<Zeroizing<String>>,
    scheme: PathScheme,
    labels: HashMap<u32, String>,
}

// The mnemonic derives every account key, so it must never surface in logs/Debug.
impl std::fmt::Debug for AccountManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountManager")
            .field("phrase", &"<redacted>")
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "<redacted>"),
            )
            .field("scheme", &self.scheme)
            .finish()
    }
}

impl AccountManager {
    /// Generate a fresh mnemonic from the OS CSPRNG. Fail-closed: an unavailable RNG is an
    /// error, never a weaker fallback. Retains the phrase (unlike alloy's `build_random`).
    pub fn generate(words: WordCount) -> Result<Self, AccountError> {
        Self::generate_with(words, &mut OsRng)
    }

    // Seam for a deterministic RNG in tests; production always uses `OsRng`.
    pub(crate) fn generate_with<R: RngCore>(
        words: WordCount,
        rng: &mut R,
    ) -> Result<Self, AccountError> {
        let mut entropy = Zeroizing::new(vec![0u8; words.entropy_len()]);
        rng.try_fill_bytes(entropy.as_mut_slice())
            .map_err(|e| AccountError::Rng(e.to_string()))?;
        let ent = Entropy::from_slice(entropy.as_slice())
            .map_err(|e| AccountError::Rng(e.to_string()))?;
        let phrase = Zeroizing::new(Mnemonic::<English>::new_from_entropy(ent).to_phrase());
        Ok(Self {
            phrase,
            passphrase: None,
            scheme: PathScheme::Bip44Standard,
            labels: HashMap::new(),
        })
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

    /// Set the BIP-39 passphrase (the "25th word"). Changes the entire address set — this is
    /// the seed passphrase, not a keystore-encryption password.
    pub fn with_passphrase(mut self, passphrase: impl Into<String>) -> Self {
        self.passphrase = Some(Zeroizing::new(passphrase.into()));
        self
    }

    /// Choose the derivation-path scheme the `index` shortcut varies (default `Bip44Standard`).
    pub fn with_scheme(mut self, scheme: PathScheme) -> Self {
        self.scheme = scheme;
        self
    }

    fn password(&self) -> Option<&str> {
        self.passphrase.as_deref().map(|s| s.as_str())
    }

    // Address at a path from the public key only — no full signer is built.
    fn address_at_path(&self, path: &str) -> Result<Address, AccountError> {
        let mnemonic = Mnemonic::<English>::new_from_phrase(self.phrase.as_str())
            .map_err(|_| AccountError::InvalidPhrase)?;
        let xpriv = mnemonic
            .derive_key(path, self.password())
            .map_err(|e| AccountError::Derivation(e.to_string()))?;
        Ok(public_key_to_address(xpriv.verify_key().as_ref()))
    }

    /// The derived account at a full derivation path (its `index` field is left 0 — the
    /// path is authoritative here).
    pub fn account_at_path(&self, path: &str) -> Result<Account, AccountError> {
        Ok(Account {
            index: 0,
            address: self.address_at_path(path)?,
            path: path.to_string(),
            label: None,
        })
    }

    /// The derived account at `index` under the active scheme.
    pub fn account(&self, index: u32) -> Result<Account, AccountError> {
        let path = self.scheme.path_for(index)?;
        Ok(Account {
            index,
            address: self.address_at_path(&path)?,
            label: self.labels.get(&index).cloned(),
            path,
        })
    }

    /// A signer for the account at a full derivation path (key stays inside alloy).
    pub fn signer_at_path(&self, path: &str) -> Result<LocalSigner, AccountError> {
        LocalSigner::from_mnemonic_path(self.phrase.as_str(), path, self.password())
            .map_err(|e| AccountError::Derivation(e.to_string()))
    }

    /// A signer for the account at `index` under the active scheme.
    pub fn signer(&self, index: u32) -> Result<LocalSigner, AccountError> {
        self.signer_at_path(&self.scheme.path_for(index)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::Signer;
    use alloy_primitives::address;

    const MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn derives_known_bip44_addresses_and_signer_matches() {
        let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
        assert_eq!(
            mgr.account(0).unwrap().address,
            address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
        assert_eq!(
            mgr.account(1).unwrap().address,
            address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8")
        );
        // The public-key address path and the alloy signer path must agree.
        assert_eq!(
            mgr.signer(1).unwrap().address(),
            mgr.account(1).unwrap().address
        );
    }

    #[test]
    fn passphrase_changes_the_address_set() {
        let base = AccountManager::from_phrase(MNEMONIC).unwrap();
        let with = AccountManager::from_phrase(MNEMONIC)
            .unwrap()
            .with_passphrase("trezor");
        assert_ne!(
            base.account(0).unwrap().address,
            with.account(0).unwrap().address
        );
    }

    #[test]
    fn ledger_live_scheme_differs_from_standard() {
        let std = AccountManager::from_phrase(MNEMONIC).unwrap();
        let led = AccountManager::from_phrase(MNEMONIC)
            .unwrap()
            .with_scheme(PathScheme::LedgerLive);
        assert_eq!(
            std.account(0).unwrap().address,
            led.account(0).unwrap().address
        ); // agree at 0
        assert_ne!(
            std.account(1).unwrap().address,
            led.account(1).unwrap().address
        ); // differ after
    }

    #[test]
    fn generate_yields_requested_word_count_and_valid_checksum() {
        let mgr = AccountManager::generate(WordCount::W24).unwrap();
        // A re-derivable account proves the generated phrase's checksum is valid.
        assert!(mgr.account(0).is_ok());
        assert_eq!(mgr.phrase.split(' ').count(), 24);
    }

    #[test]
    fn from_phrase_rejects_bad_checksum_without_leaking() {
        assert!(matches!(
            AccountManager::from_phrase("not a real mnemonic phrase at all here"),
            Err(AccountError::InvalidPhrase)
        ));
    }

    #[test]
    fn debug_redacts_the_phrase() {
        let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
        let dbg = format!("{mgr:?}");
        assert!(!dbg.contains("junk"), "phrase leaked in Debug: {dbg}");
    }
}
