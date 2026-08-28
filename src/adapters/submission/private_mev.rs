//! `PrivateMev` — the private-relay [`SubmissionStrategy`]. Broadcasts the same signed tx
//! through a private channel instead of the public mempool, so it can't be front-run.
//!
//! The route type picks the shape:
//! - [`PrivateRoute::Protect`] ([`ProtectRelay`]) is a plain `eth_sendRawTransaction`, so it
//!   reuses an alloy [`Transport`] pointed at the relay URL — retry/failover and error
//!   classification come for free.
//! - [`PrivateRoute::Flashbots`] is `eth_sendPrivateTransaction` with the
//!   `block_window`/`fast`/`hints` knobs, authed by a per-request `X-Flashbots-Signature`
//!   header alloy's provider can't attach — so this one path posts directly via `reqwest`.

use crate::adapters::Transport;
use crate::core::deps::{
    Flashbots, PrivateRoute, ProtectRelay, RouteError, Rpc, RpcError, SubmissionError,
    SubmissionOpts, SubmissionRoute, SubmissionStrategy,
};
use crate::obs::debug;
use alloy_primitives::{Bytes, TxHash, hex, keccak256};
use alloy_signer::Signer as _;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

/// Default relay endpoints; override the Protect ones with [`ProtectRelay::Custom`].
const FLASHBOTS_URL: &str = "https://relay.flashbots.net";
const MEV_BLOCKER_URL: &str = "https://rpc.mevblocker.io";
const BLOXROUTE_URL: &str = "https://protect.blxrbdn.com";

/// The private-relay counterpart to [`PublicMempool`](super::PublicMempool).
pub struct PrivateMev {
    http: reqwest::Client,
    /// The chain RPC — only for `maxBlockNumber = current + block_window`.
    rpc: Arc<dyn Rpc>,
    /// Endpoint-auth identity for the `X-Flashbots-Signature` header — rotatable, and
    /// deliberately not the transaction-signing key.
    identity: PrivateKeySigner,
    /// Per-URL Protect-relay transports, cached so each relay keeps its keep-alive client.
    protect: Mutex<HashMap<Url, Arc<dyn Rpc>>>,
}

impl PrivateMev {
    /// Build over the chain RPC (for the block window) and the endpoint-auth identity.
    pub fn new(rpc: Arc<dyn Rpc>, identity: PrivateKeySigner) -> Self {
        Self {
            http: reqwest::Client::new(),
            rpc,
            identity,
            protect: Mutex::new(HashMap::new()),
        }
    }

    /// The alloy transport for a Protect relay (built once per URL, then reused).
    fn protect_transport(&self, relay: &ProtectRelay) -> Result<Arc<dyn Rpc>, SubmissionError> {
        let url = protect_url(relay)?;
        if let Some(rpc) = self.protect.lock().get(&url).cloned() {
            return Ok(rpc);
        }
        let rpc: Arc<dyn Rpc> =
            Arc::new(
                Transport::url(url.clone()).map_err(|e| SubmissionError::RelayRejected {
                    message: format!("relay transport: {e}"),
                })?,
            );
        self.protect.lock().insert(url, rpc.clone());
        Ok(rpc)
    }

    async fn send_flashbots(
        &self,
        rlp: &Bytes,
        route: &Flashbots,
    ) -> Result<TxHash, SubmissionError> {
        let request = JsonRpc::new(
            "eth_sendPrivateTransaction",
            self.private_tx(rlp, route).await?,
        );
        // The signature must cover the exact bytes posted, so serialize once and reuse.
        let body = serde_json::to_vec(&request).map_err(encode_err)?;
        let signature = self.flashbots_signature(&body).await?;
        debug!(relay = "flashbots", "broadcasting via private relay");
        let resp = self
            .http
            .post(FLASHBOTS_URL)
            .header("Content-Type", "application/json")
            .header("X-Flashbots-Signature", signature)
            .body(body)
            .send()
            .await
            .map_err(net_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(net_err)?;
        classify_flashbots(status, &body)
    }

    /// The `eth_sendPrivateTransaction` params (Flashbots MEV-Share format).
    async fn private_tx(
        &self,
        rlp: &Bytes,
        route: &Flashbots,
    ) -> Result<PrivateTx, SubmissionError> {
        let max_block_number = match route.block_window {
            Some(window) => {
                let current = self.rpc.block_number().await?;
                Some(format!("0x{:x}", current.saturating_add(window)))
            }
            None => None,
        };
        let hints = hint_list(route);
        let preferences = (route.fast || !hints.is_empty()).then(|| Preferences {
            fast: route.fast,
            privacy: (!hints.is_empty()).then_some(Privacy { hints }),
        });
        Ok(PrivateTx {
            tx: rlp.to_string(),
            max_block_number,
            preferences,
        })
    }

    /// `X-Flashbots-Signature: address:sig` where `sig` is an EIP-191 signature over the hex
    /// of `keccak256(body)`, by the identity key.
    async fn flashbots_signature(&self, body: &[u8]) -> Result<String, SubmissionError> {
        let message = hex::encode_prefixed(keccak256(body));
        let sig = self
            .identity
            .sign_message(message.as_bytes())
            .await
            .map_err(|e| SubmissionError::RelayAuth {
                message: format!("flashbots: {e}"),
            })?;
        Ok(format!(
            "{}:{}",
            self.identity.address(),
            hex::encode_prefixed(sig.as_bytes())
        ))
    }
}

#[async_trait]
impl SubmissionStrategy for PrivateMev {
    async fn submit(
        &self,
        signed_rlp: Bytes,
        opts: &SubmissionOpts,
    ) -> Result<TxHash, SubmissionError> {
        // The Router only dispatches `Private` here.
        let SubmissionRoute::Private(route) = &opts.route else {
            return Err(SubmissionError::Route(RouteError::RelayNotConfigured));
        };
        match route {
            PrivateRoute::Flashbots(f) => self.send_flashbots(&signed_rlp, f).await,
            PrivateRoute::Protect(p) => {
                debug!(relay = ?p.relay, "broadcasting via private relay");
                // A generic Protect relay is just `eth_sendRawTransaction` — reuse alloy.
                Ok(self
                    .protect_transport(&p.relay)?
                    .send_raw(signed_rlp)
                    .await?)
            }
        }
    }

    fn supports_route(&self, route: &SubmissionRoute) -> bool {
        matches!(route, SubmissionRoute::Private(_))
    }
}

/// A JSON-RPC request carrying a single param (`params: [T]`).
#[derive(Serialize)]
struct JsonRpc<T> {
    jsonrpc: &'static str,
    id: u32,
    method: &'static str,
    params: (T,),
}

impl<T> JsonRpc<T> {
    fn new(method: &'static str, param: T) -> Self {
        Self {
            jsonrpc: "2.0",
            id: 1,
            method,
            params: (param,),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PrivateTx {
    tx: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_block_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferences: Option<Preferences>,
}

#[derive(Serialize)]
struct Preferences {
    fast: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    privacy: Option<Privacy>,
}

#[derive(Serialize)]
struct Privacy {
    hints: Vec<&'static str>,
}

/// A JSON-RPC response — `result` deserializes straight into a [`TxHash`].
#[derive(Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<TxHash>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    message: String,
}

fn protect_url(relay: &ProtectRelay) -> Result<Url, SubmissionError> {
    let raw = match relay {
        ProtectRelay::MevBlocker => MEV_BLOCKER_URL,
        ProtectRelay::Bloxroute => BLOXROUTE_URL,
        ProtectRelay::Custom(url) => return Ok(url.clone()),
    };
    raw.parse().map_err(|e| SubmissionError::RelayRejected {
        message: format!("bad relay url: {e}"),
    })
}

fn hint_list(route: &Flashbots) -> Vec<&'static str> {
    let h = &route.hints;
    [
        ("calldata", h.calldata),
        ("logs", h.logs),
        ("function_selector", h.function_selector),
        ("contract_address", h.contract_address),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect()
}

fn encode_err(e: serde_json::Error) -> SubmissionError {
    SubmissionError::Rpc(RpcError::Call {
        message: e.to_string(),
        transient: false,
    })
}

/// Map a `reqwest` transport error (connect/timeout) to a transient RPC error — the tx may
/// or may not have reached the relay, so it is treated as maybe-in-flight, never terminal.
fn net_err(e: reqwest::Error) -> SubmissionError {
    SubmissionError::Rpc(RpcError::Call {
        message: e.to_string(),
        transient: true,
    })
}

/// Turn the Flashbots HTTP response into a tx hash or a classified error. Pure (no I/O) so
/// the safety-critical mapping — a rejection must never look "sent" — is unit-tested.
fn classify_flashbots(status: StatusCode, body: &str) -> Result<TxHash, SubmissionError> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(SubmissionError::RelayAuth {
            message: format!("flashbots: {}", clip(body)),
        });
    }
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        return Err(SubmissionError::Rpc(RpcError::Call {
            message: format!("flashbots {status}: {}", clip(body)),
            transient: true,
        }));
    }
    let resp: JsonRpcResponse = serde_json::from_str(body).map_err(|_| rejected(clip(body)))?;
    if let Some(err) = resp.error {
        // A node "already known"/"nonce too low"/underpriced message settles as an RPC
        // outcome, not a hard relay rejection — reuse the canonical predicates to decide.
        let as_rpc = SubmissionError::Rpc(RpcError::Call {
            message: err.message.clone(),
            transient: false,
        });
        if as_rpc.is_already_accepted() || as_rpc.is_underpriced() {
            return Err(as_rpc);
        }
        return Err(rejected(err.message));
    }
    resp.result
        .ok_or_else(|| rejected(format!("no result in response: {}", clip(body))))
}

/// A hard Flashbots rejection carrying the relay's message.
fn rejected(message: impl std::fmt::Display) -> SubmissionError {
    SubmissionError::RelayRejected {
        message: format!("flashbots: {message}"),
    }
}

/// Bound a relay's message so a large body can't bloat an error/log.
fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_403_is_relay_auth() {
        let e = classify_flashbots(StatusCode::FORBIDDEN, "bad signature").unwrap_err();
        assert!(matches!(e, SubmissionError::RelayAuth { .. }));
        assert!(!e.is_already_accepted() && e.is_relay_terminal());
    }

    #[test]
    fn classify_decline_is_relay_rejected() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"not profitable"}}"#;
        let e = classify_flashbots(StatusCode::OK, body).unwrap_err();
        assert!(matches!(e, SubmissionError::RelayRejected { .. }));
        assert!(!e.is_already_accepted() && e.is_relay_terminal());
    }

    #[test]
    fn classify_already_known_settles_as_accepted() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"already known"}}"#;
        let e = classify_flashbots(StatusCode::OK, body).unwrap_err();
        // Must fold into the RPC path so the executor treats a rebroadcast as sent.
        assert!(e.is_already_accepted() && !e.is_relay_terminal());
    }

    #[test]
    fn classify_result_is_the_tx_hash() {
        let hash = "0x".to_string() + &"ab".repeat(32);
        let body = format!(r#"{{"jsonrpc":"2.0","id":1,"result":"{hash}"}}"#);
        let got = classify_flashbots(StatusCode::OK, &body).unwrap();
        assert_eq!(got, hash.parse::<TxHash>().unwrap());
    }

    #[test]
    fn classify_5xx_is_transient() {
        let e = classify_flashbots(StatusCode::BAD_GATEWAY, "upstream down").unwrap_err();
        assert!(e.is_transient() && !e.is_relay_terminal());
    }
}
