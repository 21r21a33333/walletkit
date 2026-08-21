use alloy_primitives::{Address, B256, Bytes, Selector, TxKind, U256, keccak256};
use serde::{Deserialize, Serialize};

/// Stable content hash of a [`TxIntent`] — the object policy, simulation, and
/// signing all bind to. A type alias, not a newtype: it is exactly a `B256` and
/// gains nothing from wrapping.
pub type IntentHash = B256;

/// A request to execute one transaction, before nonce/gas/signing are resolved.
///
/// This is the unit the whole pipeline is keyed on: its [`hash`](TxIntent::hash)
/// is what a policy approval authorizes and what the executor tracks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxIntent {
    pub chain_id: u64,
    pub account: Address,
    pub to: TxKind,
    pub value: U256,
    pub input: Bytes,
    pub purpose: Option<String>,
}

impl TxIntent {
    /// Stable content hash — the object policy/simulate/sign bind to.
    ///
    /// `serde_json` field order is fixed by struct declaration and these alloy
    /// types serialize deterministically (hex strings, no maps), so the hash is
    /// stable within a process. Phase 1 never persists it across alloy versions;
    /// if that changes, switch to explicit `alloy_rlp` encoding.
    pub fn hash(&self) -> IntentHash {
        keccak256(serde_json::to_vec(self).expect("TxIntent is serializable"))
    }

    /// The 4-byte function selector, for a `Call` carrying at least a selector's
    /// worth of calldata. `None` for value-only calls and contract creation.
    pub fn selector(&self) -> Option<Selector> {
        match self.to {
            TxKind::Call(_) if self.input.len() >= 4 => {
                Some(Selector::from_slice(&self.input[..4]))
            }
            _ => None,
        }
    }
}

/// An unforgeable, single-use authorization that a specific [`TxIntent`] passed
/// policy. Only the policy layer can [`mint`](PolicyApproval::mint) one
/// (crate-private) and it is deliberately not `Serialize`, so it cannot be
/// persisted and replayed. `Signer::sign` requires one, making the policy→sign
/// gate structural rather than conventional.
///
/// Kept minimal: the evaluation-context envelope (gas envelope, sim digest,
/// validity window, policy version) grows into it at Task 17. The approval is
/// opaque to the `Signer` port — only `authorizes`/`consume` are called — so
/// adding fields later touches no trait contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyApproval {
    intent_hash: IntentHash,
}

impl PolicyApproval {
    #[allow(dead_code)] // sole caller is the default engine (Task 8)
    pub(crate) fn mint(intent_hash: IntentHash) -> Self {
        Self { intent_hash }
    }

    pub fn authorizes(&self, intent_hash: IntentHash) -> bool {
        self.intent_hash == intent_hash
    }

    /// By value: single-use, so a leaked approval can't authorize twice.
    pub fn consume(self) -> IntentHash {
        self.intent_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> TxIntent {
        TxIntent {
            chain_id: 1,
            account: Address::from([0x11; 20]),
            to: TxKind::Call(Address::from([0x22; 20])),
            value: U256::from(1_000u64),
            input: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef, 0x00]),
            purpose: Some("swap".into()),
        }
    }

    #[test]
    fn intent_hash_binds_every_field() {
        let b = base();
        assert_eq!(b.hash(), b.clone().hash(), "hash must be deterministic");

        let mutate = |m: TxIntent| assert_ne!(b.hash(), m.hash());
        mutate(TxIntent {
            chain_id: 2,
            ..base()
        });
        mutate(TxIntent {
            account: Address::from([0x33; 20]),
            ..base()
        });
        mutate(TxIntent {
            to: TxKind::Create,
            ..base()
        });
        mutate(TxIntent {
            to: TxKind::Call(Address::from([0x44; 20])),
            ..base()
        });
        mutate(TxIntent {
            value: U256::from(1_001u64),
            ..base()
        });
        mutate(TxIntent {
            input: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef, 0x01]),
            ..base()
        });
        mutate(TxIntent {
            purpose: None,
            ..base()
        });
    }

    #[test]
    fn selector_only_for_calls_with_calldata() {
        let sel = |input: Vec<u8>, to: TxKind| {
            TxIntent {
                input: Bytes::from(input),
                to,
                ..base()
            }
            .selector()
        };
        let call = TxKind::Call(Address::from([0x22; 20]));

        assert_eq!(
            sel(vec![1, 2, 3, 4, 5], call),
            Some(Selector::from_slice(&[1, 2, 3, 4]))
        );
        assert_eq!(sel(vec![1, 2, 3], call), None, "<4 bytes of calldata");
        assert_eq!(sel(vec![], call), None, "value-only call");
        assert_eq!(
            sel(vec![1, 2, 3, 4], TxKind::Create),
            None,
            "contract creation"
        );
    }

    #[test]
    fn approval_authorizes_only_its_bound_intent() {
        let bound = base().hash();
        let other = TxIntent {
            chain_id: 999,
            ..base()
        }
        .hash();
        let approval = PolicyApproval::mint(bound);

        assert!(approval.authorizes(bound));
        assert!(!approval.authorizes(other));
        assert_eq!(approval.consume(), bound); // by value: single-use
    }
}
