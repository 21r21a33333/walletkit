//! [`ForwardRequest`] — the EIP-712 meta-transaction a user signs so a relayer can pay the
//! gas (ERC-2771). One source of truth for the signed payload: the `sol!` struct fixes the
//! typehash, and [`ForwardRequest::typed_data`] bridges it to the `Signer` port's
//! [`TypedData`] through alloy's encoder — nothing EIP-712 is hand-rolled.

use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, U256};
use alloy_sol_types::{Eip712Domain, sol};
use std::borrow::Cow;

sol! {
    // Field order IS the EIP-712 typehash. `nonce` is signed but not submitted — the
    // forwarder consumes it from its own `nonces` mapping.
    #[derive(serde::Serialize)]
    struct ForwardRequest {
        address from;
        address to;
        uint256 value;
        uint256 gas;
        uint256 nonce;
        uint48 deadline;
        bytes data;
    }

    // Emitted by `execute()` once per request. `success = false` means the forwarder verified
    // and ran but the *inner* call reverted (the nonce is still consumed) — the confirm-safety
    // signal a mined outer tx cannot convey on its own.
    event ExecutedForwardRequest(address indexed signer, uint256 nonce, bool success);
}

/// The EIP-712 domain identity of a forwarder contract. Chain id and address are per-send, so
/// only `name`/`version` distinguish one forwarder family from another (OpenZeppelin's default
/// vs. a managed relay's own forwarder). Always compile-time constants — hence `'static`.
#[derive(Debug, Clone)]
pub struct ForwarderDomain {
    /// EIP-712 domain `name`.
    pub name: Cow<'static, str>,
    /// EIP-712 domain `version`.
    pub version: Cow<'static, str>,
}

impl Default for ForwarderDomain {
    /// The OpenZeppelin `ERC2771Forwarder` v5.x defaults.
    fn default() -> Self {
        Self {
            name: Cow::Borrowed("ERC2771Forwarder"),
            version: Cow::Borrowed("1"),
        }
    }
}

impl ForwardRequest {
    /// Bind this request to `forwarder` on `chain_id` and produce the EIP-712 [`TypedData`]
    /// the policy gate authorizes and the signer signs. The `verifyingContract` pins the
    /// signature to this forwarder + chain (cross-chain-replay guard); reuses alloy's encoder
    /// via [`TypedData::from_struct`], so the typehash and hashing are never hand-rolled.
    pub fn typed_data(
        &self,
        forwarder: Address,
        chain_id: u64,
        domain: &ForwarderDomain,
    ) -> TypedData {
        let domain = Eip712Domain::new(
            Some(domain.name.clone()),
            Some(domain.version.clone()),
            Some(U256::from(chain_id)),
            Some(forwarder),
            None,
        );
        TypedData::from_struct(self, Some(domain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address, aliases::U48, b256};

    // Golden EIP-712 digest independently computed with `cast keccak`/`abi-encode` (a separate
    // implementation from alloy) for this exact request + the OZ `ERC2771Forwarder` domain, so
    // agreement cross-checks our field order and domain wiring — a wrong typehash or domain
    // would diverge.
    #[test]
    fn forward_request_eip712_matches_cast_golden() {
        let req = ForwardRequest {
            from: address!("1111111111111111111111111111111111111111"),
            to: address!("2222222222222222222222222222222222222222"),
            value: U256::ZERO,
            gas: U256::from(100_000u64),
            nonce: U256::ZERO,
            deadline: U48::from(4_102_444_800u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        };
        let forwarder = address!("3333333333333333333333333333333333333333");

        let td = req.typed_data(forwarder, 1, &ForwarderDomain::default());
        let got = crate::core::wallet::typed_data_hash(&td).expect("chain-bound domain");

        assert_eq!(
            got,
            b256!("d1882449115c3e37d2347d4a36df523be018bc2479caa841c413ccdf345c6ddb"),
        );
    }
}
