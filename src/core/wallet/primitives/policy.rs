use super::IntentHash;
use serde::{Deserialize, Serialize};

/// The fee ceiling a policy approved for an intent. A gas bump whose fees stay
/// within it reuses the same approval (no re-policy); a bump beyond it must be
/// re-evaluated. Absolute wei caps — a policy decision independent of live fees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GasEnvelope {
    pub max_fee_cap: u128,
    pub max_priority_cap: u128,
}

impl GasEnvelope {
    /// Generous mainnet default ceiling (1000 gwei fee / 50 gwei tip) — a bump rarely
    /// reaches it, so most bumps reuse the approval; a policy can tighten it.
    pub const DEFAULT: GasEnvelope = GasEnvelope {
        max_fee_cap: 1_000_000_000_000,
        max_priority_cap: 50_000_000_000,
    };

    pub fn admits(&self, max_fee: u128, max_priority: u128) -> bool {
        max_fee <= self.max_fee_cap && max_priority <= self.max_priority_cap
    }
}

/// Unforgeable proof that a specific [`TxIntent`](super::TxIntent) passed policy.
/// Minted only by the policy layer (crate-private) and not `Serialize`, so it can't
/// be persisted and replayed; `Signer::sign` requires one, making the policy→sign
/// gate structural. Bounded, not single-use: valid for any fees within
/// `gas_envelope` until `valid_until`, so the executor can bump within it without
/// re-running policy (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyApproval {
    intent_hash: IntentHash,
    gas_envelope: GasEnvelope,
    valid_until: u64,
}

impl PolicyApproval {
    /// Minted only by the policy layer (crate-private) after an intent is allowed.
    pub(crate) fn mint(
        intent_hash: IntentHash,
        gas_envelope: GasEnvelope,
        valid_until: u64,
    ) -> Self {
        Self {
            intent_hash,
            gas_envelope,
            valid_until,
        }
    }

    pub fn authorizes(&self, intent_hash: IntentHash) -> bool {
        self.intent_hash == intent_hash
    }

    pub fn gas_envelope(&self) -> GasEnvelope {
        self.gas_envelope
    }

    pub fn valid_until(&self) -> u64 {
        self.valid_until
    }
}

/// Why an engine denied an intent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("policy denied by rule `{rule}`{}: {reason}", .field.as_ref().map(|f| format!(" (field `{f}`)")).unwrap_or_default())]
pub struct PolicyRejection {
    pub rule: String,
    pub field: Option<String>,
    pub reason: String,
}

/// The verdict every policy engine returns; an operational failure is the port's
/// `Err`, not a `Decision` (fail-closed). Not `Clone` — a decision is consumed once
/// by the pipeline, which then owns the approval capability.
#[derive(Debug)]
#[non_exhaustive]
pub enum Decision {
    Allow(PolicyApproval),
    Deny(PolicyRejection),
    // RequireApproval { quorum } grows in Phase 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn approval_authorizes_only_its_bound_intent() {
        let bound = B256::from([0x11; 32]);
        let other = B256::from([0x22; 32]);
        let envelope = GasEnvelope {
            max_fee_cap: 100,
            max_priority_cap: 10,
        };
        let approval = PolicyApproval::mint(bound, envelope, 0);

        assert!(approval.authorizes(bound));
        assert!(!approval.authorizes(other));
    }

    #[test]
    fn envelope_admits_only_within_both_caps() {
        let e = GasEnvelope {
            max_fee_cap: 100,
            max_priority_cap: 10,
        };
        assert!(e.admits(100, 10));
        assert!(!e.admits(101, 10)); // over max fee
        assert!(!e.admits(100, 11)); // over priority
    }

    #[test]
    fn policy_rejection_renders_field_only_when_present() {
        let with_field = PolicyRejection {
            rule: "allowlist".into(),
            field: Some("to".into()),
            reason: "destination not allowed".into(),
        };
        assert_eq!(
            with_field.to_string(),
            "policy denied by rule `allowlist` (field `to`): destination not allowed"
        );

        let no_field = PolicyRejection {
            rule: "spend-limit".into(),
            field: None,
            reason: "over 1 ETH".into(),
        };
        assert_eq!(
            no_field.to_string(),
            "policy denied by rule `spend-limit`: over 1 ETH"
        );
    }
}
