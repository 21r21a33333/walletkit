use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// The fee ceiling a policy approved for an intent. A gas bump whose fees stay
/// within it reuses the same approval (no re-policy); a bump beyond it must be
/// re-evaluated. Absolute wei caps — a policy decision independent of live fees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GasEnvelope {
    /// Absolute cap on `max_fee_per_gas` (wei) any bump may reach.
    pub max_fee_cap: u128,
    /// Absolute cap on `max_priority_fee_per_gas` (wei) any bump may reach.
    pub max_priority_cap: u128,
}

impl GasEnvelope {
    /// Generous mainnet default ceiling (1000 gwei fee / 50 gwei tip) — a bump rarely
    /// reaches it, so most bumps reuse the approval; a policy can tighten it.
    pub const DEFAULT: GasEnvelope = GasEnvelope {
        max_fee_cap: 1_000_000_000_000,
        max_priority_cap: 50_000_000_000,
    };

    /// Whether both fees sit within their caps — the check a bump passes to reuse the approval.
    pub fn admits(&self, max_fee: u128, max_priority: u128) -> bool {
        max_fee <= self.max_fee_cap && max_priority <= self.max_priority_cap
    }
}

/// Unforgeable proof that a specific signing payload (a tx intent, an EIP-191 message, or
/// EIP-712 typed data) passed policy. Minted only by the policy layer (crate-private) and
/// not `Serialize`, so it can't be persisted and replayed; the `Signer` requires one, making
/// the policy→sign gate structural. Bounded, not single-use: valid for any fees within
/// `gas_envelope` (tx-only) until `valid_until`, so the executor can bump within it without
/// re-running policy (§5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyApproval {
    payload_hash: B256,
    gas_envelope: GasEnvelope,
    valid_until: u64,
}

impl PolicyApproval {
    /// Minted only by the policy layer (crate-private) after a payload is allowed.
    pub(crate) fn mint(payload_hash: B256, gas_envelope: GasEnvelope, valid_until: u64) -> Self {
        Self {
            payload_hash,
            gas_envelope,
            valid_until,
        }
    }

    /// Whether this approval covers `payload_hash` — the bind check the signer enforces.
    pub fn authorizes(&self, payload_hash: B256) -> bool {
        self.payload_hash == payload_hash
    }

    /// The fee ceiling within which a bump may reuse this approval without re-policy.
    pub fn gas_envelope(&self) -> GasEnvelope {
        self.gas_envelope
    }

    /// Unix seconds after which the approval expires and policy must run again.
    pub fn valid_until(&self) -> u64 {
        self.valid_until
    }
}

/// Why an engine denied an intent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("policy denied by rule `{rule}`{}: {reason}", .field.as_ref().map(|f| format!(" (field `{f}`)")).unwrap_or_default())]
pub struct PolicyRejection {
    /// Identifier of the rule that denied (e.g. `"allowlist"`, `"spend-limit"`).
    pub rule: String,
    /// The offending field, when the rule pins one (e.g. `"to"`).
    pub field: Option<String>,
    /// Human-readable explanation of the denial.
    pub reason: String,
}

/// The verdict every policy engine returns; an operational failure is the port's
/// `Err`, not a `Decision` (fail-closed). Not `Clone` — a decision is consumed once
/// by the pipeline, which then owns the approval capability.
#[derive(Debug)]
#[non_exhaustive]
pub enum Decision {
    /// Allowed — carries the unforgeable approval the signer requires.
    Allow(PolicyApproval),
    /// Denied — carries the reason.
    Deny(PolicyRejection),
}

/// The token-free result of a policy dry-run
/// ([`validate`](crate::core::deps::PolicyEngine::validate)). It deliberately carries **no**
/// [`PolicyApproval`] — a preview can never be turned into a signing capability, so the gate
/// cannot be bypassed by inspecting a validation. The deny reason mirrors a real
/// [`Decision::Deny`]. `#[non_exhaustive]` so a future `WouldRequireApproval` can land without
/// breaking callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyOutcome {
    /// The intent would pass policy (no capability is minted for a dry-run).
    WouldAllow,
    /// The intent would be denied, with the same reason a real decision carries.
    WouldDeny(PolicyRejection),
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
