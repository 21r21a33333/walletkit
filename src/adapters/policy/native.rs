use crate::core::deps::{PolicyEngine, PolicyEngineError};
use crate::core::wallet::{Decision, PolicyApproval, PolicyRejection, TxIntent};
use alloy_primitives::{Address, TxKind, U256};
use async_trait::async_trait;
use std::collections::HashSet;

/// One native rule's verdict on an intent. `Abstain` = "no opinion": a guard that
/// only speaks up (`Deny`) when tripped and never grants `Allow`.
#[derive(Debug)]
pub enum Verdict {
    Allow,
    Deny(PolicyRejection),
    Abstain,
}

/// A native, in-process policy rule. Users may implement this to add their own
/// rule; richer/declarative policy comes from the Regorus (8b) or WASM (8c)
/// engines, not new hand-written predicates here.
pub trait Policy: Send + Sync {
    fn check(&self, intent: &TxIntent) -> Verdict;
}

/// Grants `Allow` for calls to an explicitly listed target. Abstains (not denies)
/// on a non-match, so allowlists compose under default-deny — only an explicit
/// allow-rule can permit anything.
pub struct TargetAllowlist {
    allowed: HashSet<Address>,
}

impl TargetAllowlist {
    pub fn new(allowed: impl IntoIterator<Item = Address>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
        }
    }
}

impl Policy for TargetAllowlist {
    fn check(&self, i: &TxIntent) -> Verdict {
        match i.to {
            TxKind::Call(a) if self.allowed.contains(&a) => Verdict::Allow,
            _ => Verdict::Abstain,
        }
    }
}

/// Denies any intent whose value exceeds a wei-exact `U256` cap (no float/u64
/// rounding). A pure guard: abstains when within the cap.
pub struct SpendLimit {
    max_value: U256,
}

impl SpendLimit {
    pub fn new(max_value: U256) -> Self {
        Self { max_value }
    }
}

impl Policy for SpendLimit {
    fn check(&self, i: &TxIntent) -> Verdict {
        if i.value > self.max_value {
            Verdict::Deny(PolicyRejection {
                rule: "SpendLimit".into(),
                field: Some("value".into()),
                reason: format!("value {} exceeds cap {}", i.value, self.max_value),
            })
        } else {
            Verdict::Abstain
        }
    }
}

/// The zero-dependency default engine: composes native [`Policy`] rules with a
/// deny-over-allow, default-deny fold. Frozen — new capability comes from other
/// engines, not more built-in predicates.
pub struct DefaultPolicyEngine {
    policies: Vec<Box<dyn Policy>>,
}

impl DefaultPolicyEngine {
    pub fn new(policies: Vec<Box<dyn Policy>>) -> Self {
        Self { policies }
    }

    /// Any `Deny` short-circuits (deny-over-allow); otherwise at least one `Allow`
    /// is required to mint the approval bound to this intent (default-deny).
    fn decide(&self, intent: &TxIntent) -> Decision {
        let mut allowed = false;
        for p in &self.policies {
            match p.check(intent) {
                Verdict::Deny(r) => return Decision::Deny(r),
                Verdict::Allow => allowed = true,
                Verdict::Abstain => {}
            }
        }
        if allowed {
            Decision::Allow(PolicyApproval::mint(intent.hash()))
        } else {
            Decision::Deny(PolicyRejection {
                rule: "default-deny".into(),
                field: None,
                reason: "no policy granted permission".into(),
            })
        }
    }
}

#[async_trait]
impl PolicyEngine for DefaultPolicyEngine {
    async fn evaluate(&self, intent: &TxIntent) -> Result<Decision, PolicyEngineError> {
        Ok(self.decide(intent)) // native evaluation is infallible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(to: TxKind, value: U256) -> TxIntent {
        TxIntent {
            chain_id: 1,
            account: Address::ZERO,
            to,
            value,
            input: Default::default(),
            purpose: None,
        }
    }

    #[test]
    fn target_allowlist_allows_only_listed_call_targets() {
        let listed = Address::from([0xaa; 20]);
        let other = Address::from([0xbb; 20]);
        let allow = TargetAllowlist::new([listed]);

        assert!(matches!(
            allow.check(&intent(TxKind::Call(listed), U256::ZERO)),
            Verdict::Allow
        ));
        assert!(matches!(
            allow.check(&intent(TxKind::Call(other), U256::ZERO)),
            Verdict::Abstain
        ));
        assert!(matches!(
            allow.check(&intent(TxKind::Create, U256::ZERO)),
            Verdict::Abstain
        ));
    }

    #[test]
    fn spend_limit_denies_over_cap() {
        let cap = SpendLimit::new(U256::from(100u64));
        let to = TxKind::Call(Address::from([0xaa; 20]));

        match cap.check(&intent(to, U256::from(101u64))) {
            Verdict::Deny(r) => assert_eq!(r.field.as_deref(), Some("value")),
            v => panic!("expected deny, got {v:?}"),
        }
        assert!(matches!(
            cap.check(&intent(to, U256::from(100u64))),
            Verdict::Abstain
        ));
    }

    #[tokio::test]
    async fn engine_composes_allow_guard_and_default_deny() {
        let a = Address::from([0xaa; 20]);
        let b = Address::from([0xbb; 20]);
        let engine = DefaultPolicyEngine::new(vec![
            Box::new(TargetAllowlist::new([a])),
            Box::new(SpendLimit::new(U256::from(100u64))),
        ]);
        let eval =
            async |to: TxKind, value: U256| engine.evaluate(&intent(to, value)).await.unwrap();

        // allowed target within cap -> Allow
        assert!(matches!(
            eval(TxKind::Call(a), U256::from(50u64)).await,
            Decision::Allow(_)
        ));

        // deny-over-allow: allowlisted but over the cap -> Deny(SpendLimit)
        match eval(TxKind::Call(a), U256::from(200u64)).await {
            Decision::Deny(r) => assert_eq!(r.rule, "SpendLimit"),
            d => panic!("expected SpendLimit deny, got {d:?}"),
        }

        // unlisted target, nothing grants -> Deny(default-deny)
        match eval(TxKind::Call(b), U256::from(50u64)).await {
            Decision::Deny(r) => assert_eq!(r.rule, "default-deny"),
            d => panic!("expected default-deny, got {d:?}"),
        }
    }
}
