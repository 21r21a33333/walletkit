//! [`Relay`] — the gasless meta-transaction inclusion port, and [`GaslessOpts`], the per-send
//! choice of *who pays the gas*. A user-signed [`ForwardRequest`] is handed to a relay that
//! submits it and pays; the return is a [`TxHandle`] so tracking is uniform across families.
//!
//! The two families are type-state, mirroring [`submission`](super::submission): the managed
//! [`Gelato`] knobs (`FeeScheme`, `NonceScheme`) live only on `Gelato`, so a [`SelfRelay`] can't
//! carry them — an invalid combination is unrepresentable, not validated at runtime.

use crate::core::deps::{RpcError, SignerError, SubmissionError, SubmissionOpts};
use crate::core::wallet::{ForwardRequest, SignatureEnvelope, TxHandle};
use alloy_primitives::{Address, TxHash};
use async_trait::async_trait;
use std::fmt;
use std::time::Duration;

/// A user-signed [`ForwardRequest`] plus everything a relay needs to submit it: the signature
/// and the `forwarder`/`chain_id` it is bound to. Built by the wallet after the policy gate.
#[non_exhaustive]
pub struct SignedRequest {
    /// The signed forward request.
    pub request: ForwardRequest,
    /// The user's EIP-712 signature over `request`.
    pub signature: SignatureEnvelope,
    /// The forwarder the signature is bound to (the EIP-712 `verifyingContract`).
    pub forwarder: Address,
    /// The chain the request executes on.
    pub chain_id: u64,
}

impl SignedRequest {
    /// Assemble a signed request from its parts — built by the wallet after the policy gate has
    /// authorized and the user has signed the [`ForwardRequest`].
    pub(crate) fn new(
        request: ForwardRequest,
        signature: SignatureEnvelope,
        forwarder: Address,
        chain_id: u64,
    ) -> Self {
        Self {
            request,
            signature,
            forwarder,
            chain_id,
        }
    }
}

/// Gets a policy-approved, user-signed [`ForwardRequest`] included on-chain by a gas-paying
/// third party. [`SelfRelay`] submits an outer `execute()` tx we track directly; a managed
/// relay queues a task we [`poll`](Relay::poll). Either way the return is a [`TxHandle`].
#[async_trait]
pub trait Relay: Send + Sync {
    /// Relay `signed`; return a persisted [`TxHandle`] tracking inclusion.
    async fn relay(&self, signed: &SignedRequest) -> Result<TxHandle, RelayError>;

    /// Advance a task-backed handle. The default is [`RelayStatus::Settled`] — a synchronous
    /// relay already returned an on-chain hash; a managed relay overrides to poll its task API.
    async fn poll(&self, _handle: &TxHandle) -> Result<RelayStatus, RelayError> {
        Ok(RelayStatus::Settled)
    }
}

/// Where a relayed request stands. [`Included`](RelayStatus::Included) hands off to the
/// on-chain confirm path; [`Failed`](RelayStatus::Failed) is terminal at the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RelayStatus {
    /// Nothing to poll — the relay already produced an on-chain tx (self-relay).
    Settled,
    /// The managed relay has accepted the task but it is not yet included.
    Pending,
    /// The request is on-chain as this tx; chain-confirm (and the meta-tx event decode) settle it.
    Included(TxHash),
    /// The relay dropped the request (cancelled / reverted before inclusion).
    Failed(String),
}

/// Per-send gasless options. `deadline` (request expiry) is common to every family; the
/// relay-family choice carries the family-specific knobs. Not `Serialize` — [`Gelato`] holds a
/// secret (the sponsor key), which never reaches a handle, log, or span.
#[derive(Debug, Clone)]
pub struct GaslessOpts {
    /// Which relay backend gets the request included.
    pub route: GaslessRoute,
    /// How long the signed request stays valid (relative window → absolute `uint48` at build).
    pub deadline: Deadline,
}

/// The relay backend for one gasless send.
#[derive(Debug, Clone)]
pub enum GaslessRoute {
    /// Our own funded relayer submits `execute()` through the standard pipeline.
    SelfRelay(SelfRelay),
    /// A managed Gelato relay submits and pays.
    Gelato(Gelato),
}

/// Self-relay: a funded relayer key submits the outer `execute()` tx. `submission` is the
/// **outer** tx's route, so gasless composes with private submission (gasless + Flashbots).
/// Sequential-nonce by construction — the OZ standard `ERC2771Forwarder` has no salt path.
#[derive(Debug, Clone, Default)]
pub struct SelfRelay {
    /// The outer `execute()` tx's broadcast route.
    pub submission: SubmissionOpts,
}

impl SelfRelay {
    /// Self-relay with the outer tx on the public mempool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Self-relay with the outer tx on a private route (gasless + MEV protection).
    pub fn via(route: impl Into<SubmissionOpts>) -> Self {
        Self {
            submission: route.into(),
        }
    }
}

/// A managed Gelato relay. Secret-bearing (the sponsor api key), so it has a **redacting
/// `Debug`** and is never serialized.
#[derive(Clone)]
pub struct Gelato {
    /// Who ultimately pays the fee.
    pub fee: FeeScheme,
    /// The forwarder replay-protection strategy.
    pub nonce: NonceScheme,
}

impl Gelato {
    /// Sponsor the user's gas via a Gelato 1Balance api key (the user needs no tokens).
    pub fn sponsored(api_key: impl Into<String>) -> Self {
        Self {
            fee: FeeScheme::Sponsored {
                api_key: api_key.into(),
            },
            nonce: NonceScheme::default(),
        }
    }

    /// Pay the fee from the transaction's own ERC-20 (`fee_token`) during execution.
    pub fn sync_fee(fee_token: Address) -> Self {
        Self {
            fee: FeeScheme::SyncFee { fee_token },
            nonce: NonceScheme::default(),
        }
    }

    /// Use salt-based (unordered, parallel-safe) replay protection.
    pub fn concurrent(mut self) -> Self {
        self.nonce = NonceScheme::Concurrent;
        self
    }

    /// Use sequential (ordered) forwarder-nonce replay protection (the default).
    pub fn sequential(mut self) -> Self {
        self.nonce = NonceScheme::Sequential;
        self
    }
}

/// Who ultimately pays a managed relay's fee.
#[derive(Clone)]
#[non_exhaustive]
pub enum FeeScheme {
    /// The app sponsors the gas via a 1Balance api key — the user pays nothing.
    Sponsored {
        /// The Gelato sponsor api key (secret; redacted in `Debug`).
        api_key: String,
    },
    /// The fee is pulled from the transaction's own ERC-20 during execution.
    SyncFee {
        /// The ERC-20 the fee is charged in.
        fee_token: Address,
    },
}

/// Forwarder replay-protection strategy for a managed relay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NonceScheme {
    /// Ordered: a per-user on-chain nonce; request N waits for N-1.
    #[default]
    Sequential,
    /// Unordered: a unique salt per request, so independent sends run in parallel.
    Concurrent,
}

/// How long a signed request stays valid, as a window from build time; converted to an
/// absolute `uint48` deadline at build so every (re)build gets a fresh expiry.
#[derive(Debug, Clone)]
pub struct Deadline(pub Duration);

impl Default for Deadline {
    /// One hour — long enough for a relay to include, short enough to bound a stolen signature.
    fn default() -> Self {
        Self(Duration::from_secs(3600))
    }
}

impl From<SelfRelay> for GaslessOpts {
    fn from(relay: SelfRelay) -> Self {
        GaslessRoute::SelfRelay(relay).into()
    }
}

impl From<Gelato> for GaslessOpts {
    fn from(relay: Gelato) -> Self {
        GaslessRoute::Gelato(relay).into()
    }
}

impl From<GaslessRoute> for GaslessOpts {
    fn from(route: GaslessRoute) -> Self {
        // The one place the default deadline is applied; the family conversions delegate here.
        Self {
            route,
            deadline: Deadline::default(),
        }
    }
}

// Redacting `Debug`: the sponsor api key is a secret and must never reach a log/span.
impl fmt::Debug for Gelato {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gelato")
            .field("fee", &self.fee)
            .field("nonce", &self.nonce)
            .finish()
    }
}

impl fmt::Debug for FeeScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sponsored { .. } => f
                .debug_struct("Sponsored")
                .field("api_key", &"<redacted>")
                .finish(),
            Self::SyncFee { fee_token } => f
                .debug_struct("SyncFee")
                .field("fee_token", fee_token)
                .finish(),
        }
    }
}

/// Why a gasless relay operation failed; maps into [`WalletKitError`](crate::WalletKitError).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RelayError {
    /// An underlying RPC read/submit failed (transport/node) — inherits its transient split.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// The outer self-relay broadcast failed (relay-auth/rejected/etc.).
    #[error(transparent)]
    Submission(#[from] SubmissionError),
    /// Signing the request tripped the policy gate or the payload was unsignable.
    #[error(transparent)]
    Signing(#[from] SignerError),
    /// A managed relay rejected the request (bad signature, unsupported chain, sponsor
    /// exhausted). Terminal — the request never entered a task.
    #[error("relay rejected the request: {message}")]
    Rejected {
        /// The relay's reason (never secret-bearing).
        message: String,
    },
    /// The forwarder cannot be used as configured — a `nonces`/`verify` call reverted or its
    /// return didn't decode, i.e. the address isn't a conforming `ERC2771Forwarder`. Terminal.
    #[error("forwarder is not usable: {message}")]
    Forwarder {
        /// What went wrong (never secret-bearing).
        message: String,
    },
    /// A gasless send was attempted with no relayer + forwarder configured. Terminal — set them
    /// on the builder (`relayer`/`forwarder`) before `send_gasless`.
    #[error("gasless relay is not configured: set a relayer signer and a forwarder address")]
    NotConfigured,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Redaction is a security invariant: the sponsor api key must never appear in `Debug`
    // output (which can reach a log/span). Guards against a future derive re-exposing it.
    #[test]
    fn gelato_debug_redacts_the_api_key() {
        let dbg = format!("{:?}", Gelato::sponsored("super-secret-key").concurrent());
        assert!(!dbg.contains("super-secret-key"), "api key leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("Concurrent"));
    }
}
