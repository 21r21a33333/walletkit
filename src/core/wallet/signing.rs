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
///
/// `skip_all` is mandatory: the arguments carry the key-adjacent `approval`, the tx, and
/// the signature — none may become a span field. Only the safe `intent_hash` is recorded.
#[cfg_attr(
    feature = "tracing",
    tracing::instrument(name = "sign", level = "debug", skip_all, fields(intent_hash = ?intent_hash))
)]
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

// Redaction guard: signing telemetry must record only allow-listed fields, never key
// material. Runs only with the `tracing` feature (the instrumentation being guarded).
#[cfg(all(test, feature = "tracing"))]
mod redaction_tests {
    use crate::adapters::LocalSigner;
    use crate::core::deps::Signer;
    use crate::core::wallet::{GasEnvelope, PolicyApproval, TxIntent};
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    /// Collects every recorded span/event field as `"name=debug"`.
    #[derive(Default, Clone)]
    struct Capture(Arc<Mutex<Vec<String>>>);

    struct FieldSink<'a>(&'a Capture);
    impl tracing::field::Visit for FieldSink<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .0
                .lock()
                .push(format!("{}={:?}", field.name(), value));
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::span::Id,
            _: Context<'_, S>,
        ) {
            attrs.record(&mut FieldSink(self));
        }
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            event.record(&mut FieldSink(self));
        }
    }

    #[tokio::test]
    async fn signing_never_records_key_material() {
        // Anvil dev mnemonic, account 0 — its private key is KEY_HEX (asserted absent).
        let signer = LocalSigner::from_mnemonic(
            "test test test test test test test test test test test junk",
            0,
        )
        .expect("signer");

        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let intent = TxIntent {
            chain_id: 1,
            account: signer.address(),
            to: TxKind::Call(Address::from([0xbb; 20])),
            value: U256::ZERO,
            input: Bytes::new(),
            purpose: None,
        };
        let intent_hash = intent.hash();
        let approval = PolicyApproval::mint(intent_hash, GasEnvelope::DEFAULT, u64::MAX);
        let tx = super::build_tx(&intent, 0, 21_000, crate::testutils::estimation(100, 1));
        // The instrumented sign path (`#[instrument(skip_all, fields(intent_hash))]`).
        let _ = super::sign_encode(&signer, tx, intent_hash, &approval, 0)
            .await
            .expect("sign");

        const KEY_HEX: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let recorded = capture.0.lock().clone();
        // Guard against a vacuous pass: the sign span must actually have been observed.
        assert!(!recorded.is_empty(), "capture observed no sign telemetry");
        for entry in &recorded {
            assert!(
                !entry.to_lowercase().contains(KEY_HEX),
                "key leaked: {entry}"
            );
            let name = entry.split('=').next().unwrap_or("");
            // Only the allow-listed field may appear on the sign path.
            assert!(
                matches!(name, "intent_hash"),
                "unexpected sign field: {entry}"
            );
        }
    }
}
