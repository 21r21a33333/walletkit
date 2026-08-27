//! [`Signer`] — the account signing port (policy-gated, no key export).

use crate::core::wallet::{IntentHash, PolicyApproval, SignatureEnvelope, SigningError};
use alloy_consensus::TxEip1559;
use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, Signature};
use async_trait::async_trait;

/// Signs for one account. Signature-only (no key export); every method enforces the
/// [`PolicyApproval`] gate — bound payload, and not expired at `now` (plus the fee envelope
/// for a tx) — making policy→sign structural. Takes the approval by reference so a bump
/// within the envelope can reuse it (§5.1).
#[async_trait]
pub trait Signer: Send + Sync {
    /// The address this signer produces signatures for.
    fn address(&self) -> Address;

    /// Sign an EIP-1559 transaction, gated by `approval` (bound to `intent_hash`, unexpired
    /// at `now`, fees within the envelope).
    async fn sign_transaction(
        &self,
        tx: &TxEip1559,
        intent_hash: IntentHash,
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<Signature, SignerError>;

    /// Sign an EIP-191 `personal_sign` message (the `0x19` prefix is applied here).
    async fn sign_message(
        &self,
        message: &[u8],
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<SignatureEnvelope, SignerError>;

    /// Sign EIP-712 typed data (domain `chainId` validated before signing).
    async fn sign_typed_data(
        &self,
        typed: &TypedData,
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<SignatureEnvelope, SignerError>;
}

/// Why signing failed — a gate trip, a malformed payload, or a backend error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignerError {
    /// A key backend failed to load (bad hex/keystore/mnemonic, missing env var).
    #[error("failed to load signing key: {0}")]
    Load(String),
    /// The approval does not authorize the intent being signed — the gate tripped.
    #[error("policy approval does not authorize this intent")]
    ApprovalMismatch,
    /// The tx fees exceed the approved envelope — a bump must be re-evaluated by policy.
    #[error("fees exceed the approved envelope")]
    FeesExceedApproval,
    /// The approval's validity window has passed.
    #[error("policy approval expired")]
    ApprovalExpired,
    /// The signing payload is malformed (e.g. an EIP-712 zero-chain domain or a value that
    /// won't encode) — never signable as-is.
    #[error(transparent)]
    Payload(#[from] SigningError),
    /// The backend failed to produce a signature.
    #[error("signing failed: {0}")]
    Backend(String),
}
