//! [`GelatoRelay`] — the managed-relay adapter. Gelato submits the user's ERC-2771 call and pays
//! the gas; we hold only the sponsor key and poll the returned `taskId` to an on-chain hash.
//!
//! Gelato does **not** use OpenZeppelin's `ERC2771Forwarder` — it relays through its own
//! `GelatoRelay*ERC2771` forwarders, so the user signs a **different** EIP-712 struct bound to
//! Gelato's own domain (name/version/`verifyingContract` differ per fee model and nonce mode).
//! That wire format lives here and nowhere else; the OZ [`ForwardRequest`](crate::core::wallet)
//! path is self-relay-only. All field names, endpoints, domains, and the four forwarder addresses
//! are pinned from `@gelatonetwork/relay-sdk` (`master`) and confirmed end-to-end by the env-gated
//! live test — Gelato is hosted SaaS with no hermetic harness.

use crate::adapters::http;
use crate::core::deps::{
    FeeScheme, Gelato, NonceScheme, Relay, RelayError, RelayStatus, Rpc, RpcError, Simulated,
    TaskId,
};
use crate::core::wallet::{SignatureEnvelope, TxIntent};
use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, B256, Bytes, TxHash, TxKind, U256, address, hex, keccak256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::{Eip712Domain, SolCall, sol};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;

/// Gelato's REST base. The status endpoint under it is public (no key); only the relay POST
/// carries the sponsor key.
const GELATO_BASE_URL: &str = "https://api.gelato.digital";

/// The EIP-712 domain identity of one of Gelato's four ERC-2771 forwarders — its `name` and
/// `verifying_contract`. `version` is always `"1"`; the forwarder is chosen by fee model × nonce
/// mode ([`GelatoRelay::domain`]).
struct GelatoDomain {
    name: &'static str,
    verifying_contract: Address,
}

// The four Gelato ERC-2771 forwarders. Each pairs the EIP-712 domain `name` with its
// `verifyingContract` as one named constant — the single source of truth, pinned verbatim from
// `@gelatonetwork/relay-sdk`. These are the default (non-zkSync/Abstract) deployments; a chain
// with an override earns its entry when a caller targets it (YAGNI).
const SPONSORED_SEQUENTIAL: GelatoDomain = GelatoDomain {
    name: "GelatoRelay1BalanceERC2771",
    verifying_contract: address!("d8253782c45a12053594b9deb72d8e8ab2fca54c"),
};
const SPONSORED_CONCURRENT: GelatoDomain = GelatoDomain {
    name: "GelatoRelay1BalanceConcurrentERC2771",
    verifying_contract: address!("c65d82ece367ef06bf2ab791b3f3cf037dc0e816"),
};
const SYNC_FEE_SEQUENTIAL: GelatoDomain = GelatoDomain {
    name: "GelatoRelayERC2771",
    verifying_contract: address!("b539068872230f20456cf38ec52ef2f91af4ae49"),
};
const SYNC_FEE_CONCURRENT: GelatoDomain = GelatoDomain {
    name: "GelatoRelayConcurrentERC2771",
    verifying_contract: address!("8598806401a63ddf52473f1b3c55bb9e33e2d73b"),
};

sol! {
    // The four Gelato ERC-2771 signed structs. The struct *name* is the EIP-712 primary type, so
    // sponsored and syncFee need distinct names even though their fields match — the typehash
    // differs. `userNonce` (ordered) vs `userSalt` (unordered) is the sequential/concurrent split.
    // `Serialize` is required by `TypedData::from_struct` (the EIP-712 message is JSON-encoded).
    #[derive(serde::Serialize)]
    struct SponsoredCallERC2771 {
        uint256 chainId; address target; bytes data; address user; uint256 userNonce; uint256 userDeadline;
    }
    #[derive(serde::Serialize)]
    struct CallWithSyncFeeERC2771 {
        uint256 chainId; address target; bytes data; address user; uint256 userNonce; uint256 userDeadline;
    }
    #[derive(serde::Serialize)]
    struct SponsoredCallConcurrentERC2771 {
        uint256 chainId; address target; bytes data; address user; bytes32 userSalt; uint256 userDeadline;
    }
    #[derive(serde::Serialize)]
    struct CallWithSyncFeeConcurrentERC2771 {
        uint256 chainId; address target; bytes data; address user; bytes32 userSalt; uint256 userDeadline;
    }

    // Gelato's per-user sequential nonce read (its forwarder's replay counter).
    function userNonce(address user) external view returns (uint256);
}

/// The data of one Gelato ERC-2771 call, built by the adapter (which reads the sequential nonce
/// or derives the concurrent salt) and signed through the wallet's policy gate before submission.
/// Carries no secret — the sponsor key lives on [`GelatoRelay`], never on a per-send value. An
/// internal build→sign→submit handoff (the facade orchestrates it), not part of the public API.
pub(crate) struct GelatoCall {
    chain_id: u64,
    target: Address,
    data: Bytes,
    user: Address,
    replay: Replay,
    deadline: U256,
}

impl GelatoCall {
    /// The user who authorized the call (the ERC-2771 `_msgSender` the target will see).
    pub(crate) fn user(&self) -> Address {
        self.user
    }

    /// The forwarder nonce the request consumed (sequential), or `0` for a salt-based concurrent
    /// send. Recorded on the tracking handle to disambiguate, never used to gate confirmation.
    pub(crate) fn nonce(&self) -> U256 {
        match &self.replay {
            Replay::Nonce(n) => *n,
            Replay::Salt(_) => U256::ZERO,
        }
    }
}

/// Replay protection for one call: an ordered per-user nonce, or an unordered unique salt.
enum Replay {
    Nonce(U256),
    Salt(B256),
}

/// A managed Gelato relay. Holds the sponsor key/fee model + nonce mode chosen at wiring time.
/// Secret-bearing → redacting `Debug`, never serialized; only the public status endpoint is
/// polled from a persisted handle (which carries no key).
pub struct GelatoRelay {
    transport: Arc<dyn GelatoTransport>,
    fee: FeeScheme,
    nonce: NonceScheme,
    base_url: String,
}

impl GelatoRelay {
    /// Build the relay from its wiring-time [`Gelato`] configuration (sponsor key/fee model +
    /// nonce mode). One `reqwest::Client` is reused for every submit and poll.
    pub fn new(config: Gelato) -> Self {
        Self {
            transport: Arc::new(ReqwestTransport {
                http: reqwest::Client::new(),
            }),
            fee: config.fee,
            nonce: config.nonce,
            base_url: GELATO_BASE_URL.to_string(),
        }
    }

    /// Select this relay's Gelato forwarder by fee model × nonce mode.
    fn domain(&self) -> GelatoDomain {
        match (&self.fee, &self.nonce) {
            (FeeScheme::Sponsored { .. }, NonceScheme::Sequential) => SPONSORED_SEQUENTIAL,
            (FeeScheme::Sponsored { .. }, NonceScheme::Concurrent) => SPONSORED_CONCURRENT,
            (FeeScheme::SyncFee { .. }, NonceScheme::Sequential) => SYNC_FEE_SEQUENTIAL,
            (FeeScheme::SyncFee { .. }, NonceScheme::Concurrent) => SYNC_FEE_CONCURRENT,
        }
    }

    /// The forwarder the user's signature is bound to — recorded as the tracking handle's
    /// `forwarder` (context only; Gelato's own event is not decoded, its task verdict is trusted).
    pub(crate) fn verifying_contract(&self) -> Address {
        self.domain().verifying_contract
    }

    /// Bind `call` to this relay's Gelato domain and produce the EIP-712 [`TypedData`] the policy
    /// gate authorizes and the user signs. Reuses alloy's `sol!` encoder — nothing hand-rolled.
    pub(crate) fn typed_data(&self, call: &GelatoCall) -> TypedData {
        let GelatoDomain {
            name,
            verifying_contract,
        } = self.domain();
        let domain = Eip712Domain::new(
            Some(name.into()),
            Some("1".into()),
            Some(U256::from(call.chain_id)),
            Some(verifying_contract),
            None,
        );
        let chain_id = U256::from(call.chain_id);
        match (&self.fee, &call.replay) {
            (FeeScheme::Sponsored { .. }, Replay::Nonce(nonce)) => TypedData::from_struct(
                &SponsoredCallERC2771 {
                    chainId: chain_id,
                    target: call.target,
                    data: call.data.clone(),
                    user: call.user,
                    userNonce: *nonce,
                    userDeadline: call.deadline,
                },
                Some(domain),
            ),
            (FeeScheme::SyncFee { .. }, Replay::Nonce(nonce)) => TypedData::from_struct(
                &CallWithSyncFeeERC2771 {
                    chainId: chain_id,
                    target: call.target,
                    data: call.data.clone(),
                    user: call.user,
                    userNonce: *nonce,
                    userDeadline: call.deadline,
                },
                Some(domain),
            ),
            (FeeScheme::Sponsored { .. }, Replay::Salt(salt)) => TypedData::from_struct(
                &SponsoredCallConcurrentERC2771 {
                    chainId: chain_id,
                    target: call.target,
                    data: call.data.clone(),
                    user: call.user,
                    userSalt: *salt,
                    userDeadline: call.deadline,
                },
                Some(domain),
            ),
            (FeeScheme::SyncFee { .. }, Replay::Salt(salt)) => TypedData::from_struct(
                &CallWithSyncFeeConcurrentERC2771 {
                    chainId: chain_id,
                    target: call.target,
                    data: call.data.clone(),
                    user: call.user,
                    userSalt: *salt,
                    userDeadline: call.deadline,
                },
                Some(domain),
            ),
        }
    }

    /// Assemble the call: read the sequential per-user nonce from Gelato's forwarder, or derive a
    /// concurrent salt. `deadline` is the absolute `uint256` expiry. Rejects a value-bearing or
    /// contract-creation intent — ERC-2771 relaying carries neither.
    pub(crate) async fn build_call(
        &self,
        intent: &TxIntent,
        rpc: &dyn Rpc,
        deadline: U256,
    ) -> Result<GelatoCall, RelayError> {
        if !intent.value.is_zero() {
            return Err(RelayError::Rejected {
                message: "Gelato ERC-2771 relay cannot forward ETH value (msg.value must be 0)"
                    .into(),
            });
        }
        let target = match intent.to {
            TxKind::Call(to) => to,
            TxKind::Create => {
                return Err(RelayError::Rejected {
                    message: "contract creation cannot be relayed via ERC-2771".into(),
                });
            }
        };
        let replay = match self.nonce {
            NonceScheme::Sequential => Replay::Nonce(self.user_nonce(rpc, intent.account).await?),
            NonceScheme::Concurrent => {
                Replay::Salt(concurrent_salt(intent.account, &intent.input, deadline))
            }
        };
        Ok(GelatoCall {
            chain_id: intent.chain_id,
            target,
            data: intent.input.clone(),
            user: intent.account,
            replay,
            deadline,
        })
    }

    /// Read Gelato's `userNonce(user)` (sequential replay counter). A revert or undecodable
    /// return means no Gelato ERC-2771 forwarder answers at the expected address on this chain.
    async fn user_nonce(&self, rpc: &dyn Rpc, user: Address) -> Result<U256, RelayError> {
        let request = TransactionRequest {
            to: Some(TxKind::Call(self.verifying_contract())),
            input: TransactionInput::new(userNonceCall { user }.abi_encode().into()),
            ..Default::default()
        };
        match rpc.call(&request).await? {
            Simulated::Returned(bytes) => (bytes.len() >= 32)
                .then(|| U256::from_be_slice(&bytes[..32]))
                .ok_or_else(|| RelayError::Forwarder {
                    message:
                        "userNonce() returned undecodable data — not a Gelato ERC-2771 forwarder"
                            .into(),
                }),
            Simulated::Reverted(_) => Err(RelayError::Forwarder {
                message: "userNonce() reverted — no Gelato ERC-2771 forwarder at the expected \
                          address on this chain"
                    .into(),
            }),
        }
    }

    /// Submit the signed call to Gelato and return the `taskId` to poll. The sponsor key is added
    /// here (never on the call value) and never logged. `isConcurrent` and the nonce/salt field
    /// follow the relay's nonce mode; `sponsorApiKey` vs `feeToken` follows its fee model.
    pub(crate) async fn submit(
        &self,
        call: &GelatoCall,
        signature: &SignatureEnvelope,
    ) -> Result<TaskId, RelayError> {
        let mut body = Map::new();
        body.insert("chainId".into(), Value::String(call.chain_id.to_string()));
        body.insert("target".into(), Value::String(call.target.to_string()));
        body.insert("data".into(), Value::String(call.data.to_string()));
        body.insert("user".into(), Value::String(call.user.to_string()));
        body.insert(
            "userDeadline".into(),
            Value::String(call.deadline.to_string()),
        );
        body.insert(
            "userSignature".into(),
            Value::String(format!("0x{}", hex::encode(signature.as_bytes()))),
        );
        match &call.replay {
            Replay::Nonce(nonce) => {
                body.insert("userNonce".into(), Value::String(nonce.to_string()));
                body.insert("isConcurrent".into(), Value::Bool(false));
            }
            Replay::Salt(salt) => {
                body.insert("userSalt".into(), Value::String(salt.to_string()));
                body.insert("isConcurrent".into(), Value::Bool(true));
            }
        }
        match &self.fee {
            FeeScheme::Sponsored { api_key } => {
                body.insert("sponsorApiKey".into(), Value::String(api_key.clone()));
            }
            FeeScheme::SyncFee { fee_token } => {
                body.insert("feeToken".into(), Value::String(fee_token.to_string()));
                body.insert("isRelayContext".into(), Value::Bool(true));
            }
        }
        let url = format!("{}/relays/v2/{}", self.base_url, self.endpoint());
        let resp = self
            .transport
            .post_json(&url, Value::Object(body).to_string())
            .await?;
        match http::classify_status(resp.status) {
            http::HttpClass::Unauthorized => Err(RelayError::Rejected {
                message: format!("Gelato rejected credentials: {}", http::clip(&resp.body)),
            }),
            http::HttpClass::Transient => Err(RelayError::Rpc(RpcError::Call {
                message: format!("Gelato {}: {}", resp.status, http::clip(&resp.body)),
                transient: true,
            })),
            http::HttpClass::Body => {
                let parsed: RelayResponse =
                    serde_json::from_str(&resp.body).map_err(|_| RelayError::Rejected {
                        message: format!(
                            "Gelato: unexpected relay response: {}",
                            http::clip(&resp.body)
                        ),
                    })?;
                Ok(TaskId::new(parsed.task_id))
            }
        }
    }

    /// The relay endpoint under `/relays/v2` for this fee model.
    fn endpoint(&self) -> &'static str {
        match &self.fee {
            FeeScheme::Sponsored { .. } => "sponsored-call-erc2771",
            FeeScheme::SyncFee { .. } => "call-with-sync-fee-erc2771",
        }
    }
}

#[async_trait]
impl Relay for GelatoRelay {
    async fn poll(&self, task: &TaskId) -> Result<RelayStatus, RelayError> {
        let url = format!("{}/tasks/status/{}", self.base_url, task.as_str());
        let resp = self.transport.get(&url).await?;
        match http::classify_status(resp.status) {
            http::HttpClass::Unauthorized => Err(RelayError::Rejected {
                message: format!(
                    "Gelato status endpoint rejected: {}",
                    http::clip(&resp.body)
                ),
            }),
            http::HttpClass::Transient => Err(RelayError::Rpc(RpcError::Call {
                message: format!("Gelato status {}: {}", resp.status, http::clip(&resp.body)),
                transient: true,
            })),
            http::HttpClass::Body => parse_status(&resp.body),
        }
    }
}

// Redacting `Debug`: the fee model's `Debug` already hides the sponsor key; this keeps the
// transport out too so no derive can re-expose the secret.
impl fmt::Debug for GelatoRelay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GelatoRelay")
            .field("fee", &self.fee)
            .field("nonce", &self.nonce)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// A unique-per-request salt for concurrent (unordered) replay protection. Derived from the user,
/// deadline, and calldata, so distinct sends get distinct salts; callers needing N *identical*
/// concurrent sends must vary the intent (a stronger RNG salt earns its place at that consumer).
fn concurrent_salt(user: Address, data: &Bytes, deadline: U256) -> B256 {
    let mut buf = Vec::with_capacity(20 + 32 + data.len());
    buf.extend_from_slice(user.as_slice());
    buf.extend_from_slice(&deadline.to_be_bytes::<32>());
    buf.extend_from_slice(data);
    keccak256(buf)
}

/// Map a Gelato task status body to a [`RelayStatus`]. `ExecSuccess` (with a hash) is the honest
/// inclusion signal — the executor records the hash and confirms it at depth; a reverted or
/// cancelled task is terminal `Failed`; anything still in flight stays `Pending`.
fn parse_status(body: &str) -> Result<RelayStatus, RelayError> {
    let env: StatusEnvelope = serde_json::from_str(body).map_err(|_| {
        RelayError::Rpc(RpcError::Call {
            message: format!("Gelato status: unparseable body: {}", http::clip(body)),
            transient: true,
        })
    })?;
    let task = env.task;
    Ok(match task.task_state.as_str() {
        "CheckPending" | "ExecPending" | "WaitingForConfirmation" => RelayStatus::Pending,
        "ExecSuccess" => match task
            .transaction_hash
            .as_deref()
            .and_then(|hash| hash.parse::<TxHash>().ok())
        {
            Some(hash) => RelayStatus::Included(hash),
            None => RelayStatus::Pending,
        },
        "ExecReverted" | "Cancelled" | "Blacklisted" | "NotFound" => {
            RelayStatus::Failed(task.last_check_message.unwrap_or(task.task_state))
        }
        // An unrecognized state is treated as still-pending; the signed request's deadline bounds
        // the wait, so an unknown wire value can never falsely confirm or falsely fail.
        _ => RelayStatus::Pending,
    })
}

/// Gelato's relay POST response — just the task id.
#[derive(Deserialize)]
struct RelayResponse {
    #[serde(rename = "taskId")]
    task_id: String,
}

/// Gelato wraps the task status in a `task` object.
#[derive(Deserialize)]
struct StatusEnvelope {
    task: TaskStatus,
}

#[derive(Deserialize)]
struct TaskStatus {
    #[serde(rename = "taskState")]
    task_state: String,
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "lastCheckMessage")]
    last_check_message: Option<String>,
}

/// A raw HTTP response (status + body). The status classification is shared with the other
/// relay-posting adapters ([`http`]); the body parsing is Gelato-specific.
#[derive(Clone)]
struct HttpResponse {
    status: StatusCode,
    body: String,
}

/// The HTTP round-trip seam, so the wire logic is unit-tested against a stub and the live test
/// exercises the real endpoint. One method per verb Gelato needs.
#[async_trait]
trait GelatoTransport: Send + Sync {
    async fn post_json(&self, url: &str, body: String) -> Result<HttpResponse, RelayError>;
    async fn get(&self, url: &str) -> Result<HttpResponse, RelayError>;
}

/// The production transport over `reqwest`. A connect/timeout error is transient — the request
/// may or may not have reached Gelato, so it is retryable, never a terminal drop.
struct ReqwestTransport {
    http: reqwest::Client,
}

#[async_trait]
impl GelatoTransport for ReqwestTransport {
    async fn post_json(&self, url: &str, body: String) -> Result<HttpResponse, RelayError> {
        let resp = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(net_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(net_err)?;
        Ok(HttpResponse { status, body })
    }

    async fn get(&self, url: &str) -> Result<HttpResponse, RelayError> {
        let resp = self.http.get(url).send().await.map_err(net_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(net_err)?;
        Ok(HttpResponse { status, body })
    }
}

/// A `reqwest` transport error (connect/timeout) is transient — the tx may be in flight.
fn net_err(e: reqwest::Error) -> RelayError {
    RelayError::Rpc(RpcError::Call {
        message: e.to_string(),
        transient: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Signature;
    use parking_lot::Mutex;
    use std::collections::VecDeque;

    /// A stub transport that captures the POST it receives and replays canned responses, so the
    /// body construction and status mapping are tested with no network.
    struct StubTransport {
        post_url: Mutex<Option<String>>,
        post_body: Mutex<Option<String>>,
        post_response: HttpResponse,
        get_responses: Mutex<VecDeque<HttpResponse>>,
    }

    impl StubTransport {
        fn posting(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                post_url: Mutex::new(None),
                post_body: Mutex::new(None),
                post_response: response(status, body),
                get_responses: Mutex::new(VecDeque::new()),
            })
        }

        fn polling(responses: Vec<(u16, &str)>) -> Arc<Self> {
            Arc::new(Self {
                post_url: Mutex::new(None),
                post_body: Mutex::new(None),
                post_response: response(200, "{}"),
                get_responses: Mutex::new(
                    responses.into_iter().map(|(s, b)| response(s, b)).collect(),
                ),
            })
        }
    }

    #[async_trait]
    impl GelatoTransport for StubTransport {
        async fn post_json(&self, url: &str, body: String) -> Result<HttpResponse, RelayError> {
            *self.post_url.lock() = Some(url.to_string());
            *self.post_body.lock() = Some(body);
            Ok(self.post_response.clone())
        }

        async fn get(&self, _url: &str) -> Result<HttpResponse, RelayError> {
            Ok(self
                .get_responses
                .lock()
                .pop_front()
                .expect("stub get called more times than queued"))
        }
    }

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status: StatusCode::from_u16(status).expect("valid status"),
            body: body.to_string(),
        }
    }

    impl GelatoRelay {
        fn with_transport(config: Gelato, transport: Arc<dyn GelatoTransport>) -> Self {
            Self {
                transport,
                fee: config.fee,
                nonce: config.nonce,
                base_url: GELATO_BASE_URL.to_string(),
            }
        }
    }

    fn a_signature() -> SignatureEnvelope {
        SignatureEnvelope::secp256k1(
            Address::ZERO,
            Signature::new(U256::from(1u64), U256::from(1u64), false),
        )
    }

    fn call(replay: Replay) -> GelatoCall {
        GelatoCall {
            chain_id: 11_155_111,
            target: Address::repeat_byte(0x11),
            data: Bytes::from_static(&[0xab, 0xcd]),
            user: Address::repeat_byte(0x22),
            replay,
            deadline: U256::from(1_900_000_000u64),
        }
    }

    // A sponsored sequential submit must post the sponsor key + the ordered nonce to the ERC-2771
    // endpoint, mark the request non-concurrent, and echo Gelato's taskId back as the handle.
    #[tokio::test]
    async fn sponsored_sequential_submit_posts_expected_body_and_parses_task_id() {
        let stub = StubTransport::posting(201, r#"{"taskId":"0xfeed"}"#);
        let relay = GelatoRelay::with_transport(Gelato::sponsored("secret-key"), stub.clone());

        let task = relay
            .submit(&call(Replay::Nonce(U256::from(7u64))), &a_signature())
            .await
            .expect("submit ok");

        assert_eq!(task.as_str(), "0xfeed");
        let url = stub.post_url.lock().clone().expect("posted");
        assert!(
            url.ends_with("/relays/v2/sponsored-call-erc2771"),
            "url: {url}"
        );
        let body: Value =
            serde_json::from_str(&stub.post_body.lock().clone().expect("body")).expect("json");
        assert_eq!(body["userNonce"], "7");
        assert_eq!(body["isConcurrent"], false);
        assert_eq!(body["sponsorApiKey"], "secret-key");
        assert!(
            body.get("userSalt").is_none(),
            "sequential must not send a salt"
        );
        assert!(body["userSignature"].as_str().unwrap().starts_with("0x"));
    }

    // Concurrent mode replaces the ordered nonce with a unique salt and flags the request
    // concurrent — the two nonce modes must not be conflated on the wire.
    #[tokio::test]
    async fn concurrent_submit_sends_a_salt_not_a_nonce() {
        let stub = StubTransport::posting(200, r#"{"taskId":"0x01"}"#);
        let relay = GelatoRelay::with_transport(Gelato::sponsored("k").concurrent(), stub.clone());

        relay
            .submit(&call(Replay::Salt(B256::repeat_byte(0x5a))), &a_signature())
            .await
            .expect("submit ok");

        let body: Value =
            serde_json::from_str(&stub.post_body.lock().clone().expect("body")).expect("json");
        assert_eq!(body["isConcurrent"], true);
        assert!(
            body.get("userNonce").is_none(),
            "concurrent must not send a nonce"
        );
        assert_eq!(
            body["userSalt"],
            format!("0x{}", "5a".repeat(32)),
            "the unique salt must be sent verbatim"
        );
    }

    // Sequential mode must read Gelato's on-chain `userNonce` and carry it into the call; a
    // value-bearing intent has no ERC-2771 encoding and is rejected before any relay round-trip.
    #[tokio::test]
    async fn build_call_reads_the_sequential_nonce_and_rejects_value() {
        use crate::testutils::{MockRpc, u256_word};

        let relay =
            GelatoRelay::with_transport(Gelato::sponsored("k"), StubTransport::posting(200, "{}"));
        let rpc = MockRpc {
            call_returns: Some(u256_word(42)),
            ..Default::default()
        };
        let intent = TxIntent::call(
            11_155_111,
            Address::repeat_byte(0x22),
            Address::repeat_byte(0x11),
            U256::ZERO,
            Bytes::from_static(&[0xab]),
        );

        let call = relay
            .build_call(&intent, &rpc, U256::from(999u64))
            .await
            .expect("build ok");
        assert_eq!(
            call.nonce(),
            U256::from(42u64),
            "userNonce() is read into the call"
        );
        assert_eq!(call.user(), Address::repeat_byte(0x22));

        let with_value = TxIntent::call(
            1,
            Address::ZERO,
            Address::repeat_byte(0x11),
            U256::from(1u64),
            Bytes::new(),
        );
        assert!(
            matches!(
                relay.build_call(&with_value, &rpc, U256::from(1u64)).await,
                Err(RelayError::Rejected { .. })
            ),
            "a value-bearing intent cannot be relayed via ERC-2771"
        );
    }

    // The task lifecycle: still-executing polls as Pending; success yields the on-chain hash the
    // executor confirms; a cancelled task is terminal Failed — never a false inclusion.
    #[tokio::test]
    async fn poll_maps_task_states_to_relay_status() {
        let hash = format!("0x{}", "ab".repeat(32));
        let relay = GelatoRelay::with_transport(
            Gelato::sponsored("k"),
            StubTransport::polling(vec![
                (200, r#"{"task":{"taskId":"1","taskState":"ExecPending"}}"#),
                (
                    200,
                    &format!(
                        r#"{{"task":{{"taskId":"1","taskState":"ExecSuccess","transactionHash":"{hash}"}}}}"#
                    ),
                ),
                (
                    200,
                    r#"{"task":{"taskId":"1","taskState":"Cancelled","lastCheckMessage":"dropped"}}"#,
                ),
            ]),
        );
        let task = TaskId::new("1");

        assert_eq!(relay.poll(&task).await.unwrap(), RelayStatus::Pending);
        assert_eq!(
            relay.poll(&task).await.unwrap(),
            RelayStatus::Included(hash.parse().unwrap())
        );
        assert_eq!(
            relay.poll(&task).await.unwrap(),
            RelayStatus::Failed("dropped".to_string())
        );
    }
}
