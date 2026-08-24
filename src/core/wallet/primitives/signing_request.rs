//! The payload model for every signing entry point. One [`SigningRequest`] is what the
//! policy gate authorizes and what the signer signs; [`SigningRequest::signing_hash`] is the
//! value the approval binds. EIP-712 domain validation + hashing live in one place
//! ([`typed_data_hash`]) so there is a single source of truth.

use crate::core::wallet::TxIntent;
use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, Bytes, Signature, eip191_hash_message};

/// What is being signed. `#[non_exhaustive]` so more payload kinds (UserOp, 7702 auth, batch
/// calls) can slot in without breaking existing engines.
#[non_exhaustive]
pub enum SigningRequest {
    Transaction(TxIntent),
    /// A human-readable message; the EIP-191 `0x19` prefix is applied at hash time, so a
    /// signed message can never be a valid tx preimage (blind-signing guard, §5.2).
    Message(Bytes),
    TypedData(Box<TypedData>),
    /// A cancel — a 0-value self-send at a stuck nonce. Carries the intent so the gate can
    /// verify it is genuinely a self-send before default-allowing it.
    Cancel(TxIntent),
}

impl SigningRequest {
    /// The 32-byte hash the approval binds and the signer signs.
    pub fn signing_hash(&self) -> Result<B256, SigningError> {
        match self {
            Self::Transaction(intent) | Self::Cancel(intent) => Ok(intent.hash()),
            Self::Message(bytes) => Ok(eip191_hash_message(bytes)),
            Self::TypedData(td) => typed_data_hash(td),
        }
    }
}

/// EIP-712 domain validation + signing hash — the single source of truth, called both to
/// bind the approval and to sign. Rejects an absent/zero `chainId` (a domain that pins no
/// chain is a cross-chain-replay vector); exact chain / `verifyingContract` allowlisting is
/// policy's job.
pub fn typed_data_hash(td: &TypedData) -> Result<B256, SigningError> {
    match td.domain.chain_id {
        Some(id) if !id.is_zero() => {}
        _ => return Err(SigningError::ZeroChainDomain),
    }
    td.eip712_signing_hash()
        .map_err(|e| SigningError::Encode(e.to_string()))
}

/// EIP-2 low-s canonicalization — every signature this crate emits is low-s (malleability
/// guard). Already-low signatures pass through unchanged.
pub fn enforce_low_s(sig: Signature) -> Signature {
    sig.normalize_s().unwrap_or(sig)
}

/// Which curve/scheme produced a signature — carried in the envelope so a verifier never
/// assumes `ecrecover` (§5.3). `#[non_exhaustive]`: P256/passkey later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SigningScheme {
    Secp256k1Ecdsa,
}

/// A produced signature plus which key + scheme made it.
#[derive(Debug, Clone)]
pub struct SignatureEnvelope {
    scheme: SigningScheme,
    signer: Address,
    signature: Signature,
}

impl SignatureEnvelope {
    /// Wrap a secp256k1 ECDSA signature (the only scheme today) with its signer.
    pub(crate) fn secp256k1(signer: Address, signature: Signature) -> Self {
        Self {
            scheme: SigningScheme::Secp256k1Ecdsa,
            signer,
            signature,
        }
    }

    pub fn scheme(&self) -> SigningScheme {
        self.scheme
    }

    pub fn signer(&self) -> Address {
        self.signer
    }

    pub fn signature(&self) -> Signature {
        self.signature
    }

    /// The 65-byte `r‖s‖v` encoding dApps expect.
    pub fn as_bytes(&self) -> [u8; 65] {
        self.signature.as_bytes()
    }
}

/// A signing-input failure (distinct from a policy denial). Terminal — the caller must fix
/// the payload, not retry.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SigningError {
    #[error("EIP-712 domain has no (or zero) chainId — refusing to sign a chain-agnostic payload")]
    ZeroChainDomain,
    #[error("typed data could not be EIP-712 encoded: {0}")]
    Encode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed_data(chain_id: Option<u64>) -> TypedData {
        // Minimal valid EIP-712 payload; the domain chainId is the variable under test.
        let domain = match chain_id {
            Some(c) => serde_json::json!({ "chainId": c }),
            None => serde_json::json!({}),
        };
        let json = serde_json::json!({
            "types": {
                "EIP712Domain": [{ "name": "chainId", "type": "uint256" }],
                "M": [{ "name": "x", "type": "uint256" }]
            },
            "primaryType": "M",
            "domain": domain,
            "message": { "x": "1" }
        });
        serde_json::from_value(json).expect("typed data")
    }

    #[test]
    fn typed_data_hash_rejects_absent_or_zero_chain() {
        assert!(matches!(
            typed_data_hash(&typed_data(None)),
            Err(SigningError::ZeroChainDomain)
        ));
        assert!(matches!(
            typed_data_hash(&typed_data(Some(0))),
            Err(SigningError::ZeroChainDomain)
        ));
        assert!(typed_data_hash(&typed_data(Some(1))).is_ok());
    }

    #[test]
    fn enforce_low_s_output_is_canonical() {
        use alloy_primitives::U256;
        // The output is always low-s: normalize_s on it is a no-op.
        let sig = Signature::new(U256::from(1), U256::from(1), false);
        assert!(enforce_low_s(sig).normalize_s().is_none());
    }
}
