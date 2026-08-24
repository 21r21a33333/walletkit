//! MoonPay **Open Wallet Standard (OWS)** policy engine (feature `policy-moonpay`).
//!
//! Evaluates a real OWS policy JSON in two layers, exactly as OWS does:
//! 1. **declarative rules**, evaluated natively (no wasm):
//!    - `allowed_chains` — restrict to CAIP-2 chain ids (`eip155:<id>`),
//!    - `expires_at` — time-bound the authorization,
//!    - `allowed_typed_data_contracts` — EIP-712-signing-only, so it does not
//!      constrain a plain transaction intent (parsed, not applied here);
//! 2. an optional **`executable`** — a user-supplied `wasip1` plugin that receives
//!    the OWS `PolicyContext` JSON on stdin and returns `{"allow":bool,"reason"?}`
//!    on stdout. We run it in the hardened [`WasmPlugin`](super::wasm) sandbox
//!    instead of OWS's subprocess model.
//!
//! Semantics: a violated declarative rule denies immediately (deny short-circuits,
//! before the executable). If all rules pass and the executable allows, the host
//! mints the [`PolicyApproval`] — the engine never forges authorization. Any
//! executable trap/timeout/bad-output is fail-closed (`Err`).

use super::wasm::WasmPlugin;
use crate::core::deps::{PolicyEngine, PolicyEngineError};
use crate::core::wallet::{
    Decision, GasEnvelope, PolicyApproval, PolicyRejection, SigningRequest, TxIntent,
};
use alloy_primitives::TxKind;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// How long a minted approval stays valid for bumps (seconds).
const APPROVAL_TTL: u64 = 300;

/// A parsed, applicable OWS declarative rule (typed-data-only rules are dropped).
enum Rule {
    AllowedChains(HashSet<String>),
    ExpiresAtUnix(i64),
}

pub struct MoonPayPolicyEngine {
    rules: Vec<Rule>,
    executable: Option<WasmPlugin>,
    now_unix: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl MoonPayPolicyEngine {
    /// Build from an OWS policy document and an optional OWS `executable` compiled
    /// to `wasip1`. Fails (`Load`) on malformed policy JSON or a bad executable module.
    pub fn new(
        policy: serde_json::Value,
        executable: Option<&[u8]>,
    ) -> Result<Self, PolicyEngineError> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum RawRule {
            AllowedChains {
                chain_ids: Vec<String>,
            },
            ExpiresAt {
                timestamp: String,
            },
            // `allowed_typed_data_contracts` (EIP-712-only) and any future rule
            // type fall through here — they don't constrain a plain tx intent.
            #[serde(other)]
            Other,
        }
        #[derive(Deserialize)]
        struct Doc {
            #[serde(default)]
            rules: Vec<RawRule>,
        }

        let doc: Doc = serde_json::from_value(policy)
            .map_err(|e| PolicyEngineError::Load(format!("invalid OWS policy: {e}")))?;

        let mut rules = Vec::new();
        for raw in doc.rules {
            match raw {
                RawRule::AllowedChains { chain_ids } => {
                    rules.push(Rule::AllowedChains(chain_ids.into_iter().collect()))
                }
                RawRule::ExpiresAt { timestamp } => {
                    let ts = OffsetDateTime::parse(&timestamp, &Rfc3339).map_err(|e| {
                        PolicyEngineError::Load(format!("invalid expires_at {timestamp:?}: {e}"))
                    })?;
                    rules.push(Rule::ExpiresAtUnix(ts.unix_timestamp()));
                }
                RawRule::Other => {} // unmodeled rules do not constrain a tx intent
            }
        }

        Ok(Self {
            rules,
            executable: executable.map(WasmPlugin::compile).transpose()?,
            now_unix: Box::new(real_now),
        })
    }

    /// Override the time source (for `expires_at`); used by tests.
    #[cfg(test)]
    fn with_now(mut self, f: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now_unix = Box::new(f);
        self
    }
}

#[async_trait]
impl PolicyEngine for MoonPayPolicyEngine {
    async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError> {
        // Evaluates transactions and self-send cancels (a stuck-tx safety valve, subject to
        // the same rules as a tx); other payloads are default-denied.
        let intent = match request {
            SigningRequest::Transaction(i) => i,
            SigningRequest::Cancel(i) if i.is_self_send() => i,
            _ => {
                return Ok(deny(
                    "unsupported-payload",
                    None,
                    "this engine evaluates transactions and self-send cancels only".into(),
                ));
            }
        };
        let now = (self.now_unix)();

        // Declarative rules first; a violation denies before the executable runs.
        for rule in &self.rules {
            match rule {
                Rule::AllowedChains(chains) => {
                    let caip2 = format!("eip155:{}", intent.chain_id);
                    if !chains.contains(&caip2) {
                        return Ok(deny(
                            "allowed_chains",
                            Some("chain_id"),
                            format!("chain {caip2} not allowed"),
                        ));
                    }
                }
                Rule::ExpiresAtUnix(exp) => {
                    if now > *exp {
                        return Ok(deny("expires_at", None, "authorization expired".into()));
                    }
                }
            }
        }

        if let Some(plugin) = &self.executable {
            #[derive(Deserialize)]
            struct ExecDecision {
                allow: bool,
                #[serde(default)]
                reason: Option<String>,
            }
            let out = plugin.run(policy_context(intent, now).into_bytes()).await?;
            let decision: ExecDecision = serde_json::from_slice(&out).map_err(|e| {
                PolicyEngineError::Eval(format!("executable returned invalid decision: {e}"))
            })?;
            if !decision.allow {
                return Ok(deny(
                    "executable",
                    None,
                    decision.reason.unwrap_or_else(|| "denied".into()),
                ));
            }
        }

        let valid_until = now.max(0) as u64 + APPROVAL_TTL;
        Ok(Decision::Allow(PolicyApproval::mint(
            intent.hash(),
            GasEnvelope::DEFAULT,
            valid_until,
        )))
    }
}

fn deny(rule: &str, field: Option<&str>, reason: String) -> Decision {
    Decision::Deny(PolicyRejection {
        rule: rule.into(),
        field: field.map(Into::into),
        reason,
    })
}

fn real_now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// The OWS `PolicyContext` handed to the executable on stdin.
fn policy_context(intent: &TxIntent, now_unix: i64) -> String {
    let to = match intent.to {
        TxKind::Call(a) => Some(format!("{a:#x}")),
        TxKind::Create => None,
    };
    json!({
        "chain_id": format!("eip155:{}", intent.chain_id),
        "transaction": {
            "to": to,
            "value": intent.value.to_string(),
            "data": format!("0x{}", hex(&intent.input)),
        },
        "timestamp": now_unix,
    })
    .to_string()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};

    const NOW_2026: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z, between the test expiries

    // Tiny wasip1 "executable" plugins, compiled from WAT at test time (wasmtime
    // accepts WAT) — no committed binary or fixture crate. `unreachable`/`loop`
    // exercise the sandbox; the writer emits a fixed decision on stdout.
    const TRAP_WAT: &str = r#"(module (func (export "_start") unreachable))"#;
    const LOOP_WAT: &str = r#"(module (func (export "_start") (loop (br 0))))"#;

    fn writer_wat(body: &str) -> String {
        format!(
            r#"(module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 16) "{esc}")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 16))
                (i32.store (i32.const 4) (i32.const {len}))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))"#,
            esc = body.replace('"', "\\\""),
            len = body.len(),
        )
    }

    fn intent(chain_id: u64, to: Address) -> TxIntent {
        TxIntent {
            chain_id,
            account: Address::ZERO,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: Default::default(),
            purpose: None,
        }
    }

    fn tx(chain_id: u64, to: Address) -> SigningRequest {
        SigningRequest::Transaction(intent(chain_id, to))
    }

    fn doc(rules: serde_json::Value) -> serde_json::Value {
        json!({ "id": "p", "name": "n", "version": 1, "created_at": "2026-01-01T00:00:00Z", "rules": rules, "action": "deny" })
    }

    fn with_exec(rules: serde_json::Value, wat: &str) -> MoonPayPolicyEngine {
        MoonPayPolicyEngine::new(doc(rules), Some(wat.as_bytes())).unwrap()
    }

    fn to() -> Address {
        Address::from([0xaa; 20])
    }

    #[tokio::test]
    async fn allowed_chains_denies_other_chains() {
        let engine = MoonPayPolicyEngine::new(
            doc(json!([{ "type": "allowed_chains", "chain_ids": ["eip155:8453"] }])),
            None,
        )
        .unwrap();

        assert!(matches!(
            engine.evaluate(&tx(8453, to())).await.unwrap(),
            Decision::Allow(_)
        ));
        match engine.evaluate(&tx(1, to())).await.unwrap() {
            Decision::Deny(r) => assert_eq!(r.rule, "allowed_chains"),
            d => panic!("expected allowed_chains deny, got {d:?}"),
        }
    }

    #[tokio::test]
    async fn expires_at_denies_after_expiry() {
        let expired = MoonPayPolicyEngine::new(
            doc(json!([{ "type": "expires_at", "timestamp": "2020-01-01T00:00:00Z" }])),
            None,
        )
        .unwrap()
        .with_now(|| NOW_2026);
        let valid = MoonPayPolicyEngine::new(
            doc(json!([{ "type": "expires_at", "timestamp": "2030-01-01T00:00:00Z" }])),
            None,
        )
        .unwrap()
        .with_now(|| NOW_2026);

        match expired.evaluate(&tx(1, to())).await.unwrap() {
            Decision::Deny(r) => assert_eq!(r.rule, "expires_at"),
            d => panic!("expected expiry deny, got {d:?}"),
        }
        assert!(matches!(
            valid.evaluate(&tx(1, to())).await.unwrap(),
            Decision::Allow(_)
        ));
    }

    #[tokio::test]
    async fn executable_allow_and_deny() {
        let allow = with_exec(json!([]), &writer_wat(r#"{"allow":true}"#));
        let deny = with_exec(json!([]), &writer_wat(r#"{"allow":false}"#));

        assert!(matches!(
            allow.evaluate(&tx(1, to())).await.unwrap(),
            Decision::Allow(_)
        ));
        match deny.evaluate(&tx(1, to())).await.unwrap() {
            Decision::Deny(r) => assert_eq!(r.rule, "executable"),
            d => panic!("expected executable deny, got {d:?}"),
        }
    }

    #[tokio::test]
    async fn declarative_deny_precedes_executable() {
        // On a disallowed chain the (allowing) executable is never consulted.
        let engine = with_exec(
            json!([{ "type": "allowed_chains", "chain_ids": ["eip155:8453"] }]),
            &writer_wat(r#"{"allow":true}"#),
        );
        match engine.evaluate(&tx(1, to())).await.unwrap() {
            Decision::Deny(r) => assert_eq!(r.rule, "allowed_chains"),
            d => panic!("expected allowed_chains deny, got {d:?}"),
        }
    }

    #[tokio::test]
    async fn executable_trap_is_fail_closed() {
        let engine = with_exec(json!([]), TRAP_WAT);
        assert!(matches!(
            engine.evaluate(&tx(1, to())).await,
            Err(PolicyEngineError::Eval(_))
        ));
    }

    #[tokio::test]
    async fn runaway_executable_is_fuel_trapped() {
        let engine = with_exec(json!([]), LOOP_WAT);
        assert!(matches!(
            engine.evaluate(&tx(1, to())).await,
            Err(PolicyEngineError::Eval(_))
        ));
    }

    #[tokio::test]
    async fn self_send_cancel_is_evaluated_like_a_tx() {
        let engine = MoonPayPolicyEngine::new(
            doc(json!([{ "type": "allowed_chains", "chain_ids": ["eip155:1"] }])),
            None,
        )
        .unwrap();
        // A self-send cancel (to == account == ZERO) rides the tx rules — allowed on eip155:1.
        assert!(matches!(
            engine
                .evaluate(&SigningRequest::Cancel(intent(1, Address::ZERO)))
                .await
                .unwrap(),
            Decision::Allow(_)
        ));
        // A non-self-send Cancel is not a real cancel — still default-denied.
        assert!(matches!(
            engine
                .evaluate(&SigningRequest::Cancel(intent(1, to())))
                .await
                .unwrap(),
            Decision::Deny(_)
        ));
    }
}
