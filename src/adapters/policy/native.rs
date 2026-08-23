use crate::core::deps::{Clock, PolicyEngine, PolicyEngineError};
use crate::core::wallet::{Decision, GasEnvelope, PolicyApproval, PolicyRejection, SigningRequest};
use alloy_primitives::{Address, TxKind, U256};
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;

/// How long an approval stays valid for bumps (seconds).
const DEFAULT_APPROVAL_TTL: u64 = 300;

/// One native rule's verdict on a signing request. `Abstain` = "no opinion": a guard that
/// only speaks up (`Deny`) when tripped and never grants `Allow`.
#[derive(Debug)]
pub enum Verdict {
    Allow,
    Deny(PolicyRejection),
    Abstain,
}

/// A native, in-process policy rule. Users may implement this to add their own
/// rule; richer/declarative policy comes from the Regorus or WASM engines, not new
/// hand-written predicates here.
pub trait Policy: Send + Sync {
    fn check(&self, request: &SigningRequest) -> Verdict;
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
    fn check(&self, request: &SigningRequest) -> Verdict {
        // Abstains on non-tx and unlisted targets alike, so allowlists compose under default-deny.
        match request {
            SigningRequest::Transaction(i) if matches!(i.to, TxKind::Call(a) if self.allowed.contains(&a)) => {
                Verdict::Allow
            }
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
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::Transaction(i) if i.value > self.max_value => {
                Verdict::Deny(PolicyRejection {
                    rule: "SpendLimit".into(),
                    field: Some("value".into()),
                    reason: format!("value {} exceeds cap {}", i.value, self.max_value),
                })
            }
            _ => Verdict::Abstain,
        }
    }
}

/// Opt-in for EIP-191 message signing. Coarse by design: a `0x19`-prefixed message can
/// never be a valid tx preimage, so blanket message signing is low-risk. Abstains on
/// non-messages.
pub struct MessageSigningAllowed;

impl Policy for MessageSigningAllowed {
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::Message(_) => Verdict::Allow,
            _ => Verdict::Abstain,
        }
    }
}

/// Allows EIP-712 signing only for listed `verifyingContract`s (the Permit2/Seaport guard).
/// Abstains otherwise, so unknown domains stay default-denied.
pub struct TypedDataDomainAllowlist {
    allowed: HashSet<Address>,
}

impl TypedDataDomainAllowlist {
    pub fn new(allowed: impl IntoIterator<Item = Address>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
        }
    }
}

impl Policy for TypedDataDomainAllowlist {
    fn check(&self, request: &SigningRequest) -> Verdict {
        match request {
            SigningRequest::TypedData(td)
                if td
                    .domain
                    .verifying_contract
                    .is_some_and(|c| self.allowed.contains(&c)) =>
            {
                Verdict::Allow
            }
            _ => Verdict::Abstain,
        }
    }
}

/// The zero-dependency default engine: composes native [`Policy`] rules with a
/// deny-over-allow, default-deny fold. Frozen — new capability comes from other
/// engines, not more built-in predicates.
pub struct DefaultPolicyEngine {
    policies: Vec<Box<dyn Policy>>,
    clock: Arc<dyn Clock>,
    fee_caps: GasEnvelope,
    approval_ttl: u64,
}

impl DefaultPolicyEngine {
    pub fn new(policies: Vec<Box<dyn Policy>>, clock: Arc<dyn Clock>) -> Self {
        Self {
            policies,
            clock,
            fee_caps: GasEnvelope::DEFAULT,
            approval_ttl: DEFAULT_APPROVAL_TTL,
        }
    }

    /// Override the approved fee ceiling (default ≈ 1000/50 gwei).
    pub fn with_fee_caps(mut self, fee_caps: GasEnvelope) -> Self {
        self.fee_caps = fee_caps;
        self
    }

    /// Any `Deny` short-circuits (deny-over-allow); otherwise at least one `Allow` is
    /// required to mint the approval bound to this request's payload (default-deny). A
    /// payload that won't hash is fail-closed as a deny.
    fn decide(&self, request: &SigningRequest) -> Decision {
        let mut allowed = false;
        for p in &self.policies {
            match p.check(request) {
                Verdict::Deny(r) => return Decision::Deny(r),
                Verdict::Allow => allowed = true,
                Verdict::Abstain => {}
            }
        }
        if !allowed {
            return Decision::Deny(PolicyRejection {
                rule: "default-deny".into(),
                field: None,
                reason: "no policy granted permission".into(),
            });
        }
        let Ok(payload_hash) = request.signing_hash() else {
            return Decision::Deny(PolicyRejection {
                rule: "malformed-payload".into(),
                field: None,
                reason: "signing payload could not be hashed".into(),
            });
        };
        let valid_until = self.clock.now_unix() + self.approval_ttl;
        Decision::Allow(PolicyApproval::mint(
            payload_hash,
            self.fee_caps,
            valid_until,
        ))
    }
}

#[async_trait]
impl PolicyEngine for DefaultPolicyEngine {
    async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError> {
        Ok(self.decide(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::wallet::TxIntent;

    struct FixedClock;
    impl Clock for FixedClock {
        fn now_unix(&self) -> u64 {
            1_000
        }
    }

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

    fn tx(to: TxKind, value: U256) -> SigningRequest {
        SigningRequest::Transaction(intent(to, value))
    }

    #[test]
    fn target_allowlist_allows_only_listed_call_targets() {
        let listed = Address::from([0xaa; 20]);
        let other = Address::from([0xbb; 20]);
        let allow = TargetAllowlist::new([listed]);

        assert!(matches!(
            allow.check(&tx(TxKind::Call(listed), U256::ZERO)),
            Verdict::Allow
        ));
        assert!(matches!(
            allow.check(&tx(TxKind::Call(other), U256::ZERO)),
            Verdict::Abstain
        ));
        assert!(matches!(
            allow.check(&tx(TxKind::Create, U256::ZERO)),
            Verdict::Abstain
        ));
    }

    #[test]
    fn spend_limit_denies_over_cap() {
        let cap = SpendLimit::new(U256::from(100u64));
        let to = TxKind::Call(Address::from([0xaa; 20]));

        match cap.check(&tx(to, U256::from(101u64))) {
            Verdict::Deny(r) => assert_eq!(r.field.as_deref(), Some("value")),
            v => panic!("expected deny, got {v:?}"),
        }
        assert!(matches!(
            cap.check(&tx(to, U256::from(100u64))),
            Verdict::Abstain
        ));
    }

    #[tokio::test]
    async fn engine_composes_allow_guard_and_default_deny() {
        let a = Address::from([0xaa; 20]);
        let b = Address::from([0xbb; 20]);
        let engine = DefaultPolicyEngine::new(
            vec![
                Box::new(TargetAllowlist::new([a])),
                Box::new(SpendLimit::new(U256::from(100u64))),
            ],
            Arc::new(FixedClock),
        );
        let eval = async |to: TxKind, value: U256| engine.evaluate(&tx(to, value)).await.unwrap();

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

    fn typed_data(verifying_contract: Address) -> SigningRequest {
        let json = serde_json::json!({
            "types": {
                "EIP712Domain": [
                    { "name": "chainId", "type": "uint256" },
                    { "name": "verifyingContract", "type": "address" }
                ],
                "M": [{ "name": "x", "type": "uint256" }]
            },
            "primaryType": "M",
            "domain": { "chainId": 1, "verifyingContract": verifying_contract },
            "message": { "x": "1" }
        });
        SigningRequest::TypedData(Box::new(serde_json::from_value(json).expect("typed data")))
    }

    #[tokio::test]
    async fn message_and_typed_data_are_default_denied_without_a_rule() {
        // Tx-shaped rules abstain on non-tx payloads, so default-deny protects them.
        let engine = DefaultPolicyEngine::new(
            vec![Box::new(TargetAllowlist::new([]))],
            Arc::new(FixedClock),
        );
        assert!(matches!(
            engine
                .evaluate(&SigningRequest::Message(vec![1, 2, 3].into()))
                .await
                .unwrap(),
            Decision::Deny(_)
        ));
        assert!(matches!(
            engine
                .evaluate(&typed_data(Address::from([0xcc; 20])))
                .await
                .unwrap(),
            Decision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn message_signing_allowed_grants_only_messages() {
        let engine =
            DefaultPolicyEngine::new(vec![Box::new(MessageSigningAllowed)], Arc::new(FixedClock));
        assert!(matches!(
            engine
                .evaluate(&SigningRequest::Message(b"hi".to_vec().into()))
                .await
                .unwrap(),
            Decision::Allow(_)
        ));
        // A message rule does not grant a transaction.
        assert!(matches!(
            engine
                .evaluate(&tx(TxKind::Call(Address::ZERO), U256::ZERO))
                .await
                .unwrap(),
            Decision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn typed_data_domain_allowlist_grants_only_listed_contracts() {
        let listed = Address::from([0xcc; 20]);
        let other = Address::from([0xdd; 20]);
        let engine = DefaultPolicyEngine::new(
            vec![Box::new(TypedDataDomainAllowlist::new([listed]))],
            Arc::new(FixedClock),
        );
        assert!(matches!(
            engine.evaluate(&typed_data(listed)).await.unwrap(),
            Decision::Allow(_)
        ));
        assert!(matches!(
            engine.evaluate(&typed_data(other)).await.unwrap(),
            Decision::Deny(_)
        ));
    }
}
