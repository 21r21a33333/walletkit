//! `AccountManager` — a seed-owning HD account factory. Holds one BIP-39 mnemonic in
//! zeroizing memory and derives accounts/signers under it. A derived account's private key
//! is materialized only inside alloy's signer (never exported here), so the F1
//! signature-only invariant holds; this type only *constructs* signers.

use crate::adapters::LocalSigner;
use crate::core::accounts::{
    Account, AccountError, AccountXpub, DiscoveredAccounts, DiscoveryOpts, PathScheme,
    UsedPredicate, WordCount,
};
use crate::core::deps::{AccountActivity, Rpc};
use alloy_primitives::Address;
use alloy_signer::utils::public_key_to_address;
use coins_bip39::{English, Entropy, Mnemonic};
use rand::RngCore;
use rand::rngs::OsRng;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
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

    /// The account-level extended public key `m/44'/60'/{account}'`. Addresses under it
    /// (`crate::core::accounts::derive_address`) derive without the private key — hand it to
    /// a watch-only service instead of the seed.
    pub fn account_xpub(&self, account: u32) -> Result<AccountXpub, AccountError> {
        let path = format!("m/44'/60'/{account}'");
        let mnemonic = Mnemonic::<English>::new_from_phrase(self.phrase.as_str())
            .map_err(|_| AccountError::InvalidPhrase)?;
        let xpriv = mnemonic
            .derive_key(path.as_str(), self.password())
            .map_err(|e| AccountError::Derivation(e.to_string()))?;
        Ok(AccountXpub(xpriv.verify_key()))
    }

    /// Scan the seed for used accounts across `chains` (union): derive index `i` per scheme,
    /// mark it used if `nonce > 0` (and, for `NonceOrBalance`, `|| native_balance > 0`) on any
    /// chain, and stop after `gap_limit` consecutive unused. Indices are probed in
    /// `gap_limit`-sized windows, each a **single batched round-trip per chain**
    /// ([`account_activity`](crate::core::deps::Rpc::account_activity)). Always explicit —
    /// never run on construction, since a scan reveals the whole address set to the RPC.
    pub async fn discover(
        &self,
        chains: &[Arc<dyn Rpc>],
        opts: DiscoveryOpts,
    ) -> Result<DiscoveredAccounts, AccountError> {
        let mut found: BTreeMap<Address, Account> = BTreeMap::new();
        let mut scanned_to = 0u32;
        let mut hit_max_index = false;
        let mut partial = false;
        let window = opts.gap_limit.max(1); // look ahead a full gap at a time

        for scheme in &opts.schemes {
            let mut consecutive_unused = 0usize;
            let mut i = opts.start_index;
            'scheme: while i < opts.max_index {
                let end = (i + window).min(opts.max_index);
                let mut entries: Vec<(u32, String, Address)> = Vec::with_capacity(end - i);
                for x in i..end {
                    let idx = x as u32;
                    let path = scheme.path_for(idx)?;
                    let address = self.address_at_path(&path)?;
                    entries.push((idx, path, address));
                }
                let addrs: Vec<Address> = entries.iter().map(|e| e.2).collect();

                // Union "used" across chains, one batch per chain. A chain outage can only
                // hide usage, never invent it — flag partial and leave those entries unused.
                let mut used = vec![false; addrs.len()];
                for chain in chains {
                    match chain.account_activity(&addrs).await {
                        Ok(activity) => {
                            for (k, act) in activity.iter().enumerate() {
                                if is_used(act, opts.used) {
                                    used[k] = true;
                                }
                            }
                        }
                        Err(_) => partial = true,
                    }
                }

                for (k, (idx, path, address)) in entries.into_iter().enumerate() {
                    scanned_to = scanned_to.max(idx);
                    if used[k] {
                        consecutive_unused = 0;
                        found.entry(address).or_insert(Account {
                            index: idx,
                            label: self.labels.get(&idx).cloned(),
                            address,
                            path,
                        });
                    } else {
                        consecutive_unused += 1;
                        if consecutive_unused >= opts.gap_limit {
                            break 'scheme;
                        }
                    }
                }

                // The bound, not the gap, ended the scan → the result is partial.
                if end == opts.max_index {
                    hit_max_index = true;
                    break 'scheme;
                }
                i = end;
            }
        }

        let mut accounts: Vec<Account> = found.into_values().collect();
        accounts.sort_by_key(|a| a.index);
        Ok(DiscoveredAccounts {
            accounts,
            scanned_to,
            hit_max_index,
            partial,
        })
    }
}

/// Whether one account's activity counts as "used" under the predicate.
fn is_used(act: &AccountActivity, pred: UsedPredicate) -> bool {
    act.nonce > 0 || (pred == UsedPredicate::NonceOrBalance && !act.balance.is_zero())
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

    #[test]
    fn watch_only_xpub_derives_same_addresses_as_the_seed() {
        use crate::core::accounts::derive_address;
        let mgr = AccountManager::from_phrase(MNEMONIC).unwrap();
        let xpub = mgr.account_xpub(0).unwrap();
        // m/44'/60'/0' + 0/{index} == m/44'/60'/0'/0/{index} == account(index) under Bip44Standard,
        // proving the public-key-only path matches full seed derivation.
        for i in 0..3u32 {
            assert_eq!(
                derive_address(&xpub, i).unwrap(),
                mgr.account(i).unwrap().address
            );
        }
    }
}
