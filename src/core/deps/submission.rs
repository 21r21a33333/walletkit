//! [`SubmissionStrategy`] — the transaction-broadcast port, and [`SubmissionOpts`], the
//! per-send routing choice (public mempool vs. a private/MEV-protected relay).

use crate::core::deps::RpcError;
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

/// Per-send routing options. `Default` = the public mempool (the pre-Phase-2 behavior),
/// so an unset field or a legacy persisted record routes publicly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionOpts {
    /// Which channel broadcasts the signed transaction.
    pub route: SubmissionRoute,
}

/// The broadcast channel for one send.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmissionRoute {
    /// `eth_sendRawTransaction` to the public mempool.
    #[default]
    Public,
    /// Broadcast through a private relay (MEV protection).
    Private(PrivateRoute),
}

/// Private-relay routing knobs. Public fields, matching the `DiscoveryOpts` idiom; the
/// Flashbots-only knobs (`block_window`/`fast`/`hints`) on a generic relay are rejected by
/// [`validate`](PrivateRoute::validate) at the send boundary, never dropped silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateRoute {
    /// Which private relay to broadcast through.
    pub relay: Relay,
    /// What the bump loop does if the tx does not land (no default — an explicit choice).
    pub escalation: Escalation,
    /// Inclusion window as blocks-ahead; converted to an absolute `maxBlockNumber` at each
    /// submit so bumps and recovery always recompute a fresh window. Flashbots-only.
    pub block_window: Option<u64>,
    /// MEV-Share fast inclusion. Flashbots-only.
    pub fast: bool,
    /// What to reveal to searchers for backrun rebates. Flashbots-only.
    pub hints: Hints,
}

impl PrivateRoute {
    /// A private route with no Flashbots-only knobs set — valid on any relay.
    pub fn new(relay: Relay, escalation: Escalation) -> Self {
        Self {
            relay,
            escalation,
            block_window: None,
            fast: false,
            hints: Hints::default(),
        }
    }

    /// Reject Flashbots-only knobs on a generic Protect relay. Called at `send_with` before
    /// the persist-before-broadcast write, so an unsupported combination fails the send
    /// cleanly rather than being silently ignored at broadcast time.
    pub fn validate(&self) -> Result<(), RouteError> {
        let flashbots_only =
            self.block_window.is_some() || self.fast || self.hints != Hints::default();
        if flashbots_only && !matches!(self.relay, Relay::Flashbots) {
            return Err(RouteError::GenericRelayOptions {
                relay: self.relay.clone(),
            });
        }
        Ok(())
    }
}

/// A private-relay endpoint, modeled like the RPC [`Vendor`](crate::adapters::Vendor) enum.
/// Only [`Flashbots`](Relay::Flashbots) supports the `eth_sendPrivateTransaction` knobs; the
/// rest are generic Protect RPCs reached by a plain raw send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Relay {
    /// Flashbots Protect (native `eth_sendPrivateTransaction`).
    Flashbots,
    /// MEV Blocker (CoW) Protect RPC.
    MevBlocker,
    /// bloXroute Protect RPC.
    Bloxroute,
    /// A custom Protect-RPC endpoint.
    Custom(Url),
}

/// What the executor's bump loop does when a private tx has not landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Escalation {
    /// Re-sign at a higher tip and re-send through the same relay — never leaks to public.
    StayPrivate,
    /// Fall through to the public mempool after this many private bump cycles.
    PublicAfter {
        /// Private bump cycles to attempt before escalating.
        cycles: u8,
    },
}

/// MEV-Share disclosure flags — what a private tx reveals to searchers for backrun rebates.
/// `Default` reveals nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    /// Reveal the full calldata.
    pub calldata: bool,
    /// Reveal emitted logs.
    pub logs: bool,
    /// Reveal the 4-byte function selector.
    pub function_selector: bool,
    /// Reveal the target contract address.
    pub contract_address: bool,
}

/// A private route configured with options the chosen relay cannot honor.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteError {
    /// A generic Protect relay carried Flashbots-only knobs (`block_window`/`fast`/`hints`).
    #[error("{relay:?} is a generic Protect relay; block_window/fast/hints are Flashbots-only")]
    GenericRelayOptions {
        /// The relay that cannot honor the knobs.
        relay: Relay,
    },
}

/// Broadcasts a signed, RLP-encoded transaction and returns its hash. `opts` selects the
/// route; the same signed bytes go out either way.
#[async_trait]
pub trait SubmissionStrategy: Send + Sync {
    /// Broadcast `signed_rlp` via the route in `opts` and return the transaction hash.
    async fn submit(
        &self,
        signed_rlp: Bytes,
        opts: &SubmissionOpts,
    ) -> Result<TxHash, SubmissionError>;
}

/// Why a broadcast failed; its predicates classify the failure for the executor.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmissionError {
    /// The underlying RPC broadcast call failed.
    #[error(transparent)]
    Rpc(#[from] RpcError),
}

impl SubmissionError {
    /// Transient/indeterminate (network/timeout/5xx/rate-limit): the tx may already be
    /// in flight, so the caller may assume it was sent rather than reject it.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Rpc(RpcError::Call {
                transient: true,
                ..
            })
        )
    }

    /// The node already accepted this tx or its nonce ("already known" / "nonce too
    /// low"): it is effectively sent or mined, not rejected — keep the nonce and let
    /// the executor's confirm settle it.
    pub fn is_already_accepted(&self) -> bool {
        // JSON-RPC has no structured code for these, so match the canonical geth/reth
        // messages (case-insensitively).
        const ALREADY_ACCEPTED: [&str; 3] = ["already known", "already imported", "nonce too low"];
        match self {
            Self::Rpc(RpcError::Call { message, .. }) => {
                let message = message.to_ascii_lowercase();
                ALREADY_ACCEPTED.iter().any(|m| message.contains(m))
            }
        }
    }

    /// A replacement rejected as underpriced ("replacement transaction underpriced"): a
    /// competing tx at this nonce out-bids ours. Retryable — re-price higher and resend.
    pub fn is_underpriced(&self) -> bool {
        match self {
            Self::Rpc(RpcError::Call { message, .. }) => message
                .to_ascii_lowercase()
                .contains("replacement transaction underpriced"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_flashbots_knobs_on_a_generic_relay() {
        let mut generic = PrivateRoute::new(Relay::MevBlocker, Escalation::StayPrivate);
        generic.fast = true;
        assert!(matches!(
            generic.validate(),
            Err(RouteError::GenericRelayOptions { .. })
        ));

        // The same knobs on Flashbots are valid.
        let flashbots = PrivateRoute {
            relay: Relay::Flashbots,
            escalation: Escalation::StayPrivate,
            block_window: Some(25),
            fast: true,
            hints: Hints {
                calldata: true,
                ..Hints::default()
            },
        };
        assert!(flashbots.validate().is_ok());

        // A generic relay with no Flashbots-only knobs is valid.
        assert!(
            PrivateRoute::new(Relay::Bloxroute, Escalation::StayPrivate)
                .validate()
                .is_ok()
        );
    }
}
