//! Shared tx assembly used by both the send pipeline and the executor's bump — the
//! one place that turns intent fields into a signed, 2718-encoded transaction.

use crate::core::deps::{Signer, SignerError};
use crate::core::wallet::{IntentHash, PolicyApproval, TxIntent};
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, TxHash};

/// Assemble the EIP-1559 tx for an intent at a given nonce/gas/fees.
pub(crate) fn build_tx(
    intent: &TxIntent,
    nonce: u64,
    gas_limit: u64,
    fees: Eip1559Estimation,
) -> TxEip1559 {
    TxEip1559 {
        chain_id: intent.chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas: fees.max_fee_per_gas,
        max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
        to: intent.to,
        value: intent.value,
        input: intent.input.clone(),
        access_list: Default::default(),
    }
}

/// Sign through the [`Signer`] gate and 2718-encode, returning the raw rlp and its
/// tx hash. The gate (bound intent, envelope, non-expiry) lives in the signer.
pub(crate) async fn sign_encode(
    signer: &dyn Signer,
    tx: TxEip1559,
    intent_hash: IntentHash,
    approval: &PolicyApproval,
    now: u64,
) -> Result<(Bytes, TxHash), SignerError> {
    let signature = signer
        .sign_transaction(&tx, intent_hash, approval, now)
        .await?;
    let signed = tx.into_signed(signature);
    let hash = *signed.hash();
    Ok((Bytes::from(signed.encoded_2718()), hash))
}
