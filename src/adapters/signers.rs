//! `Signer` backends over alloy's [`PrivateKeySigner`]. The loaders (raw key, env
//! var, keystore file, HD mnemonic) are just ways to load the same key type. "Key
//! never leaves" is structural: the [`Signer`] port has no export method and the
//! private key stays inside alloy.

use crate::core::deps::{Signer, SignerError};
use crate::core::wallet::{
    IntentHash, PolicyApproval, SignatureEnvelope, enforce_low_s, typed_data_hash,
};
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, Signature, eip191_hash_message};
use alloy_signer::SignerSync;
use alloy_signer_local::coins_bip39::English;
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner};
use async_trait::async_trait;
use std::path::Path;
use zeroize::Zeroizing;

pub struct LocalSigner {
    inner: PrivateKeySigner, // holds the key; no export
}

impl LocalSigner {
    /// Load from a raw `0x`-hex private key.
    pub fn from_private_key(hex: &str) -> Result<Self, SignerError> {
        Ok(Self {
            inner: hex.parse().map_err(load)?,
        })
    }

    /// Read a private key from an environment variable (zeroized after parsing).
    pub fn from_env(var: &str) -> Result<Self, SignerError> {
        let secret = Zeroizing::new(
            std::env::var(var).map_err(|e| SignerError::Load(format!("env `{var}`: {e}")))?,
        );
        Self::from_private_key(secret.as_str())
    }

    /// Decrypt a Web3 Secret Storage (JSON) keystore file.
    pub fn from_keystore(path: &Path, password: &str) -> Result<Self, SignerError> {
        Ok(Self {
            inner: PrivateKeySigner::decrypt_keystore(path, password).map_err(load)?,
        })
    }

    /// Derive from a BIP-39 mnemonic at BIP-44 `m/44'/60'/0'/0/{index}`.
    pub fn from_mnemonic(phrase: &str, index: u32) -> Result<Self, SignerError> {
        let inner = MnemonicBuilder::<English>::default()
            .phrase(phrase)
            .index(index)
            .map_err(load)?
            .build()
            .map_err(load)?;
        Ok(Self { inner })
    }

    /// The structural gate shared by every signing method: the approval must bind this
    /// payload and not be expired. (A tx additionally checks the fee envelope.)
    fn gate(
        &self,
        approval: &PolicyApproval,
        payload_hash: B256,
        now: u64,
    ) -> Result<(), SignerError> {
        if !approval.authorizes(payload_hash) {
            return Err(SignerError::ApprovalMismatch);
        }
        if now > approval.valid_until() {
            return Err(SignerError::ApprovalExpired);
        }
        Ok(())
    }

    /// Sign a prehash and enforce EIP-2 low-s. Local signing is CPU-only, so the sync path
    /// avoids an executor round-trip.
    fn sign_hash_low_s(&self, hash: &B256) -> Result<Signature, SignerError> {
        let sig = self
            .inner
            .sign_hash_sync(hash)
            .map_err(|e| SignerError::Backend(e.to_string()))?;
        Ok(enforce_low_s(sig))
    }
}

#[async_trait]
impl Signer for LocalSigner {
    fn address(&self) -> Address {
        self.inner.address()
    }

    async fn sign_transaction(
        &self,
        tx: &TxEip1559,
        intent_hash: IntentHash,
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<Signature, SignerError> {
        self.gate(approval, intent_hash, now)?;
        // A tx additionally must price within the approved envelope; a bump beyond it is
        // re-evaluated by policy.
        if !approval
            .gas_envelope()
            .admits(tx.max_fee_per_gas, tx.max_priority_fee_per_gas)
        {
            return Err(SignerError::FeesExceedApproval);
        }
        self.sign_hash_low_s(&tx.signature_hash())
    }

    async fn sign_message(
        &self,
        message: &[u8],
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<SignatureEnvelope, SignerError> {
        let hash = eip191_hash_message(message);
        self.gate(approval, hash, now)?;
        Ok(SignatureEnvelope::secp256k1(
            self.address(),
            self.sign_hash_low_s(&hash)?,
        ))
    }

    async fn sign_typed_data(
        &self,
        typed: &TypedData,
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<SignatureEnvelope, SignerError> {
        let hash = typed_data_hash(typed)?; // domain-chainId guard, single source of truth
        self.gate(approval, hash, now)?;
        Ok(SignatureEnvelope::secp256k1(
            self.address(),
            self.sign_hash_low_s(&hash)?,
        ))
    }
}

fn load(e: impl std::fmt::Display) -> SignerError {
    SignerError::Load(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::wallet::GasEnvelope;
    use alloy_primitives::{B256, address};

    // Foundry/Hardhat default test mnemonic + its first two derived accounts.
    const MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn from_mnemonic_derives_the_bip44_accounts() {
        assert_eq!(
            LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap().address(),
            address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        );
        assert_eq!(
            LocalSigner::from_mnemonic(MNEMONIC, 1).unwrap().address(),
            address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8"),
        );
    }

    #[tokio::test]
    async fn signs_only_within_the_approval_gate() {
        let signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
        let tx = |max_fee: u128| TxEip1559 {
            chain_id: 1,
            max_fee_per_gas: max_fee,
            ..Default::default()
        };
        let intent = B256::from([0x11; 32]);
        let other = B256::from([0x22; 32]);
        let envelope = GasEnvelope {
            max_fee_cap: 100,
            max_priority_cap: 100,
        };
        let approval = |bound, valid_until| PolicyApproval::mint(bound, envelope, valid_until);

        // bound intent, fees within envelope, unexpired -> signs.
        assert!(
            signer
                .sign_transaction(&tx(50), intent, &approval(intent, 1000), 0)
                .await
                .is_ok()
        );
        // wrong intent, over-envelope fees, and expiry each trip the gate.
        assert!(matches!(
            signer
                .sign_transaction(&tx(50), intent, &approval(other, 1000), 0)
                .await,
            Err(SignerError::ApprovalMismatch)
        ));
        assert!(matches!(
            signer
                .sign_transaction(&tx(200), intent, &approval(intent, 1000), 0)
                .await,
            Err(SignerError::FeesExceedApproval)
        ));
        assert!(matches!(
            signer
                .sign_transaction(&tx(50), intent, &approval(intent, 0), 1)
                .await,
            Err(SignerError::ApprovalExpired)
        ));
    }

    fn approval_for(bound: B256) -> PolicyApproval {
        PolicyApproval::mint(bound, GasEnvelope::DEFAULT, u64::MAX)
    }

    #[tokio::test]
    async fn signs_message_recovers_to_signer_and_is_low_s() {
        let signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
        let msg = b"login to example.com";
        let env = signer
            .sign_message(msg, &approval_for(eip191_hash_message(msg)), 0)
            .await
            .unwrap();
        // Recovering via EIP-191 proves the 0x19 prefix was applied.
        assert_eq!(
            env.signature().recover_address_from_msg(msg).unwrap(),
            signer.address()
        );
        assert!(env.signature().normalize_s().is_none(), "low-s");
    }

    #[tokio::test]
    async fn signs_typed_data_recovers_to_signer() {
        let signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
        let json = serde_json::json!({
            "types": {
                "EIP712Domain": [{ "name": "chainId", "type": "uint256" }],
                "M": [{ "name": "x", "type": "uint256" }]
            },
            "primaryType": "M",
            "domain": { "chainId": 1 },
            "message": { "x": "1" }
        });
        let typed: TypedData = serde_json::from_value(json).unwrap();
        let hash = typed_data_hash(&typed).unwrap();
        let env = signer
            .sign_typed_data(&typed, &approval_for(hash), 0)
            .await
            .unwrap();
        assert_eq!(
            env.signature().recover_address_from_prehash(&hash).unwrap(),
            signer.address()
        );
    }

    #[tokio::test]
    async fn sign_message_trips_the_gate_on_wrong_payload() {
        let signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
        assert!(matches!(
            signer
                .sign_message(b"x", &approval_for(B256::ZERO), 0)
                .await,
            Err(SignerError::ApprovalMismatch)
        ));
    }
}
