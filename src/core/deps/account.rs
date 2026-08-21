use crate::core::wallet::TxIntent;
use alloy_consensus::TxEip1559;
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::Signature;

/// Assembles the unsigned transaction from an intent plus a resolved nonce and
/// fee estimate, and hands out a stub signature so a tx can be simulated before
/// it is actually signed. EOA only in Phase 1.
///
/// Infallible: assembly is pure and total, so no error type. (Signing, which can
/// fail, lives on [`Signer`](super::Signer).)
pub trait Account: Send + Sync {
    fn build_unsigned(&self, intent: &TxIntent, nonce: u64, fees: Eip1559Estimation) -> TxEip1559;
    fn stub_signature(&self) -> Signature;
}
