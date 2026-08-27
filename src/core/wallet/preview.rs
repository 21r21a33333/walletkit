//! `TxPreview` — an RPC-only pre-sign simulation: gas estimate, success or a decoded
//! revert reason, EIP-2930 access list, and raw return data. Composed from `eth_call` +
//! `eth_estimateGas` + `eth_createAccessList`; never signs or mutates state. Gas is
//! advisory (a dry-run is a lower bound), and a **revert is a successful preview** with a
//! `Revert` outcome — not an error.

use crate::core::deps::{Rpc, RpcError, Simulated};
use crate::core::wallet::TxIntent;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{AccessList, TransactionInput, TransactionRequest};
use alloy_sol_types::{Panic, Revert, SolError};

/// The outcome of simulating an intent without signing it.
#[derive(Debug)]
#[non_exhaustive]
pub struct TxPreview {
    /// `eth_estimateGas` — advisory (a dry-run is a lower bound); `None` when the tx would
    /// revert (there is no meaningful estimate).
    pub gas_estimate: Option<u64>,
    /// Whether the simulated call succeeded or reverted (with the decoded reason).
    pub outcome: SimOutcome,
    /// EIP-2930 access list — the addresses/slots the call touches; `None` when the node
    /// doesn't support `eth_createAccessList` or it failed.
    pub access_list: Option<AccessList>,
    /// Raw `eth_call` return data; the caller ABI-decodes it if it expects a value.
    pub return_data: Bytes,
}

/// Whether a simulated call would succeed or revert.
#[derive(Debug)]
#[non_exhaustive]
pub enum SimOutcome {
    /// The call would succeed.
    Success,
    /// The call would revert, with the decoded reason.
    Revert(RevertReason),
}

/// A decoded `eth_call` revert. Standard selectors are named; anything else keeps its raw
/// bytes for the caller to interpret (RPC-only — no provider needed to decode).
#[derive(Debug)]
#[non_exhaustive]
pub enum RevertReason {
    /// `Error(string)` — selector `0x08c379a0`.
    Error(String),
    /// `Panic(uint256)` — selector `0x4e487b71`; carries the panic code.
    Panic(u64),
    /// A contract's custom error: 4-byte selector + ABI-encoded tail.
    Custom {
        /// The 4-byte error selector.
        selector: [u8; 4],
        /// The ABI-encoded error arguments.
        data: Bytes,
    },
    /// Empty or non-decodable revert data.
    Unknown(Bytes),
}

/// Decode raw revert bytes into a [`RevertReason`].
pub fn decode_revert(data: &Bytes) -> RevertReason {
    let Some(selector) = data.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return RevertReason::Unknown(data.clone());
    };
    if selector == Revert::SELECTOR {
        return match Revert::abi_decode(data) {
            Ok(r) => RevertReason::Error(r.reason),
            Err(_) => RevertReason::Unknown(data.clone()),
        };
    }
    if selector == Panic::SELECTOR {
        return match Panic::abi_decode(data) {
            Ok(p) => RevertReason::Panic(p.code.saturating_to::<u64>()),
            Err(_) => RevertReason::Unknown(data.clone()),
        };
    }
    RevertReason::Custom {
        selector,
        data: data.clone(),
    }
}

/// Simulate an intent over `rpc` without signing: `eth_call` (outcome + return data),
/// `eth_estimateGas` (advisory), `eth_createAccessList` (access list). A revert on the call
/// is a `Revert` outcome, not an error; the gas/access-list extras degrade to `None` on
/// failure (e.g. a reverting tx has no gas estimate) without masking the outcome.
pub(crate) async fn dry_run(rpc: &dyn Rpc, intent: &TxIntent) -> Result<TxPreview, RpcError> {
    let request = TransactionRequest {
        from: Some(intent.account),
        to: Some(intent.to),
        value: Some(intent.value),
        input: TransactionInput::new(intent.input.clone()),
        ..Default::default()
    };

    let (outcome, return_data) = match rpc.call(&request).await? {
        Simulated::Returned(data) => (SimOutcome::Success, data),
        Simulated::Reverted(data) => (SimOutcome::Revert(decode_revert(&data)), data),
    };

    let gas_estimate = rpc.estimate_gas(&request).await.ok();
    let access_list = rpc
        .create_access_list(&request)
        .await
        .ok()
        .map(|r| r.access_list);

    Ok(TxPreview {
        gas_estimate,
        outcome,
        access_list,
        return_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, U256};
    use alloy_sol_types::{Panic, Revert, SolError};

    #[test]
    fn decodes_error_panic_custom_and_unknown() {
        // Error(string)
        let e = Bytes::from(Revert::from("boom").abi_encode());
        assert!(matches!(decode_revert(&e), RevertReason::Error(s) if s == "boom"));

        // Panic(uint256) with code 0x11 (arithmetic overflow)
        let p = Bytes::from(
            Panic {
                code: U256::from(0x11),
            }
            .abi_encode(),
        );
        assert!(matches!(decode_revert(&p), RevertReason::Panic(0x11)));

        // An unknown custom-error selector keeps its raw selector + tail.
        let mut custom = vec![0xaa, 0xbb, 0xcc, 0xdd];
        custom.extend_from_slice(&[0u8; 32]);
        assert!(matches!(
            decode_revert(&Bytes::from(custom)),
            RevertReason::Custom {
                selector: [0xaa, 0xbb, 0xcc, 0xdd],
                ..
            }
        ));

        // Empty / opaque revert data.
        assert!(matches!(
            decode_revert(&Bytes::new()),
            RevertReason::Unknown(_)
        ));
    }
}
