//! [`SubmissionStrategy`] — the transaction-broadcast port, and [`SubmissionOpts`], the
//! per-send routing choice (public mempool vs. a private/MEV-protected relay).
//!
//! Routes are type-state: the Flashbots-only knobs (`block_window`/`fast`/`hints`) live only
//! on [`Flashbots`], so a generic [`Protect`] relay structurally cannot carry them — an
//! invalid combination is unrepresentable rather than validated at runtime.

use crate::core::deps::RpcError;
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

/// Per-send routing options. `Default` = the public mempool, so an unset field or a legacy
/// persisted record routes publicly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionOpts {
    /// Which channel broadcasts the signed transaction.
    pub route: SubmissionRoute,
}

impl SubmissionOpts {
    /// Route through the public mempool (the default).
    pub fn public() -> Self {
        Self::default()
    }
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

/// A private broadcast route. The two relay families differ in capability, so they are
/// distinct types — the richer Flashbots knobs cannot be attached to a generic relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivateRoute {
    /// Flashbots-native (`eth_sendPrivateTransaction`), with the inclusion knobs.
    Flashbots(Flashbots),
    /// A generic Protect RPC (`eth_sendRawTransaction` to the relay).
    Protect(Protect),
}

impl PrivateRoute {
    /// The stuck-tx behavior, common to both relay families.
    pub fn escalation(&self) -> &Escalation {
        match self {
            Self::Flashbots(f) => &f.escalation,
            Self::Protect(p) => &p.escalation,
        }
    }
}

/// A Flashbots-native private route. Build it with [`Flashbots::new`], then layer knobs:
/// `Flashbots::new(Escalation::StayPrivate).fast().within(25)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flashbots {
    /// What the bump loop does if the tx does not land.
    pub escalation: Escalation,
    /// Inclusion window as blocks-ahead; converted to an absolute `maxBlockNumber` at each
    /// submit so bumps and recovery always recompute a fresh window.
    pub block_window: Option<u64>,
    /// MEV-Share fast inclusion.
    pub fast: bool,
    /// What to reveal to searchers for backrun rebates.
    pub hints: Hints,
}

impl Flashbots {
    /// A Flashbots route with no knobs set.
    pub fn new(escalation: Escalation) -> Self {
        Self {
            escalation,
            block_window: None,
            fast: false,
            hints: Hints::default(),
        }
    }

    /// Give up private inclusion after `blocks` blocks (`maxBlockNumber`).
    pub fn within(mut self, blocks: u64) -> Self {
        self.block_window = Some(blocks);
        self
    }

    /// Request MEV-Share fast inclusion.
    pub fn fast(mut self) -> Self {
        self.fast = true;
        self
    }

    /// Reveal the given hints to searchers (for backrun rebates).
    pub fn reveal(mut self, hints: Hints) -> Self {
        self.hints = hints;
        self
    }
}

/// A generic Protect-RPC private route — relay plus stuck-tx behavior, no Flashbots knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protect {
    /// Which Protect relay to broadcast through.
    pub relay: ProtectRelay,
    /// What the bump loop does if the tx does not land.
    pub escalation: Escalation,
}

impl Protect {
    /// Route through MEV Blocker (CoW).
    pub fn mev_blocker(escalation: Escalation) -> Self {
        Self {
            relay: ProtectRelay::MevBlocker,
            escalation,
        }
    }

    /// Route through bloXroute Protect.
    pub fn bloxroute(escalation: Escalation) -> Self {
        Self {
            relay: ProtectRelay::Bloxroute,
            escalation,
        }
    }

    /// Route through a custom Protect-RPC endpoint.
    pub fn custom(url: Url, escalation: Escalation) -> Self {
        Self {
            relay: ProtectRelay::Custom(url),
            escalation,
        }
    }
}

/// A generic Protect-RPC endpoint (no `eth_sendPrivateTransaction` knobs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProtectRelay {
    /// MEV Blocker (CoW).
    MevBlocker,
    /// bloXroute Protect.
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

impl From<Flashbots> for SubmissionOpts {
    fn from(f: Flashbots) -> Self {
        Self {
            route: SubmissionRoute::Private(PrivateRoute::Flashbots(f)),
        }
    }
}

impl From<Protect> for SubmissionOpts {
    fn from(p: Protect) -> Self {
        Self {
            route: SubmissionRoute::Private(PrivateRoute::Protect(p)),
        }
    }
}

impl From<SubmissionRoute> for SubmissionOpts {
    fn from(route: SubmissionRoute) -> Self {
        Self { route }
    }
}

/// Why a route could not be honored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteError {
    /// A private route was requested but the wallet has no relay identity configured, so it
    /// cannot route privately. Fails the send rather than leak the tx to the public mempool.
    #[error("private routing requested but no relay identity is configured")]
    RelayNotConfigured,
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

    /// Whether this strategy can broadcast `route`. The default accepts anything; routing
    /// combinators narrow it (a router with no private relay wired rejects `Private`). The
    /// pipeline checks this up front so an unroutable send fails before allocating a nonce.
    fn supports_route(&self, _route: &SubmissionRoute) -> bool {
        true
    }
}

/// Why a broadcast failed; its predicates classify the failure for the executor.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmissionError {
    /// The underlying RPC broadcast call failed.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// A private relay rejected our endpoint-auth identity (a bad, rotated, or expired
    /// signing key). Not transient and not "sent" — a configuration error; the tx did not
    /// go out.
    #[error("relay rejected the endpoint-auth identity: {message}")]
    RelayAuth {
        /// The relay's message (relay name included).
        message: String,
    },
    /// A private relay declined inclusion (profitability/simulation/policy). Terminal for
    /// this relay; the executor escalates per `Escalation` rather than assume the tx was
    /// broadcast.
    #[error("relay declined the transaction: {message}")]
    RelayRejected {
        /// The relay's message (relay name included).
        message: String,
    },
    /// The requested route could not be honored (e.g. no relay configured).
    #[error(transparent)]
    Route(#[from] RouteError),
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
            _ => false,
        }
    }

    /// A replacement rejected as underpriced ("replacement transaction underpriced"): a
    /// competing tx at this nonce out-bids ours. Retryable — re-price higher and resend.
    pub fn is_underpriced(&self) -> bool {
        match self {
            Self::Rpc(RpcError::Call { message, .. }) => message
                .to_ascii_lowercase()
                .contains("replacement transaction underpriced"),
            _ => false,
        }
    }

    /// A private relay refused the tx (bad auth identity or declined inclusion): the tx
    /// definitely did not broadcast, so it must not be treated as sent. The executor may
    /// escalate per `Escalation` rather than track a phantom in-flight tx.
    pub fn is_relay_terminal(&self) -> bool {
        matches!(self, Self::RelayAuth { .. } | Self::RelayRejected { .. })
    }
}
