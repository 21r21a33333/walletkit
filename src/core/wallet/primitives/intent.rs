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
    /// A value-only transfer of `value` wei from `account` to `to`.
    pub fn transfer(chain_id: u64, account: Address, to: Address, value: U256) -> Self {
        Self {
            chain_id,
            account,
            to: TxKind::Call(to),
            value,
            input: Bytes::new(),
            purpose: None,
        }
    }

    /// A contract call to `to` carrying `input` calldata (and optional `value`).
    pub fn call(chain_id: u64, account: Address, to: Address, value: U256, input: Bytes) -> Self {
        Self {
            chain_id,
            account,
            to: TxKind::Call(to),
            value,
            input,
            purpose: None,
        }
    }

    /// Stable content hash policy/simulate/sign bind to. `serde_json` is
    /// deterministic for these alloy types (fixed field order, hex strings), so the
    /// hash is stable within a process — but it is not persisted across alloy
    /// versions; switch to `alloy_rlp` if that ever changes.
    pub fn hash(&self) -> IntentHash {
        // Infallible: `TxIntent` is a plain struct of alloy scalars with no map keys or
        // custom `Serialize`, and the `Vec` sink cannot do I/O — `to_vec` never errors.
        #[allow(clippy::expect_used)]
        let bytes = serde_json::to_vec(self).expect("TxIntent is serializable");
        keccak256(bytes)
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

    /// A 0-value self-send with empty calldata (EIP-2831 `tx_cancel`; viem/ethers'
    /// `cancelled` predicate).
    pub(crate) fn is_self_send(&self) -> bool {
        self.to == TxKind::Call(self.account) && self.value.is_zero() && self.input.is_empty()
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
    fn constructors_wrap_the_recipient_as_a_call() {
        let acct = Address::from([0x11; 20]);
        let to = Address::from([0x22; 20]);
        let t = TxIntent::transfer(1, acct, to, U256::from(5u64));
        assert_eq!(
            (t.to, t.value, t.input.is_empty()),
            (TxKind::Call(to), U256::from(5u64), true)
        );
        let c = TxIntent::call(1, acct, to, U256::ZERO, Bytes::from(vec![0xab, 0xcd]));
        assert_eq!(c.to, TxKind::Call(to));
        assert_eq!(c.input, Bytes::from(vec![0xab, 0xcd]));
    }
}
