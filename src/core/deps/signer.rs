use crate::core::wallet::{IntentHash, PolicyApproval};
use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, Signature};
use async_trait::async_trait;

/// Signs transactions for one account. Signature-only (no key export), and the
/// single-use [`PolicyApproval`] must authorize exactly `intent_hash` — the signer
/// enforces that bind, making the policy→sign gate structural rather than a
/// convention a caller can skip.
#[async_trait]
pub trait Signer: Send + Sync {
    fn address(&self) -> Address;

    async fn sign_transaction(
        &self,
        tx: &TxEip1559,
        intent_hash: IntentHash,
        approval: PolicyApproval,
    ) -> Result<Signature, SignerError>;
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignerError {
    /// A key backend failed to load (bad hex/keystore/mnemonic, missing env var).
    #[error("failed to load signing key: {0}")]
    Load(String),
    /// The approval does not authorize the intent being signed — the gate tripped.
    #[error("policy approval does not authorize this intent")]
    ApprovalMismatch,
    /// The backend failed to produce a signature.
    #[error("signing failed: {0}")]
    Backend(String),
}
