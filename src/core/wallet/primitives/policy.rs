use super::IntentHash;

/// Unforgeable, single-use proof that a specific [`TxIntent`](super::TxIntent)
/// passed policy. Minted only by the policy layer (crate-private) and not
/// `Serialize`, so it can't be persisted and replayed; `Signer::sign` requires
/// one, making the policy→sign gate structural. Grows an evaluation-context
/// envelope at Task 17.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyApproval {
    intent_hash: IntentHash,
}

impl PolicyApproval {
    /// Minted only by the policy layer (crate-private) after an intent is allowed.
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

/// Why an engine denied an intent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("policy denied by rule `{rule}`{}: {reason}", .field.as_ref().map(|f| format!(" (field `{f}`)")).unwrap_or_default())]
pub struct PolicyRejection {
    pub rule: String,
    pub field: Option<String>,
    pub reason: String,
}

/// The verdict every policy engine returns; an operational failure is the port's
/// `Err`, not a `Decision` (fail-closed). Not `Clone` — `Allow` holds a single-use
/// capability that must not be duplicated.
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
        let approval = PolicyApproval::mint(bound);

        assert!(approval.authorizes(bound));
        assert!(!approval.authorizes(other));
        assert_eq!(approval.consume(), bound);
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
