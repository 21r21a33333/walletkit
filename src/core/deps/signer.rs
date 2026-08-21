use crate::core::wallet::PolicyApproval;
use alloy_consensus::TxEip1559;
use alloy_primitives::{Address, Signature};
use async_trait::async_trait;

/// Signs transactions for one account. Signature-only (no key export), and every
/// signature requires a [`PolicyApproval`] — that argument is what makes the
/// policy→sign gate structural rather than a convention a caller can skip.
#[async_trait]
pub trait Signer: Send + Sync {
    fn address(&self) -> Address;

    async fn sign_transaction(
        &self,
        tx: &TxEip1559,
        approval: PolicyApproval,
    ) -> Result<Signature, SignerError>;
}

/// Variants grow with the signing adapters that produce them (env / keystore / HD, Task 11).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignerError {}
