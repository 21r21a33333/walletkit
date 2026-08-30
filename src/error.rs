//! The public error taxonomy (SPEC §5.5). Per-port `{Trait}Error`s remain the internal
//! contracts; `WalletKitError` is the one umbrella every `Wallet` operation returns,
//! classified for retry with a machine-readable [`ErrorKind`].

use crate::core::accounts::AccountError;
use crate::core::deps::{
    EnsError, GasOracleError, NonceManagerError, PolicyEngineError, ReadError, RelayError,
    RouteError, RpcError, SignerError, StateStoreError, SubmissionError,
};
use crate::core::wallet::{ExecutorError, PolicyRejection, TransactionManagerError};
use alloy_primitives::Address;
use std::time::Duration;

/// Machine-readable retry classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A transient failure — retrying the same request may succeed.
    Retryable,
    /// A permanent failure — retrying will not help.
    Terminal,
    /// The tx may already be in flight or the chain moved — reconcile before acting.
    NeedsReconcile,
}

/// The one error every `Wallet` operation surfaces.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WalletKitError {
    /// An RPC call failed.
    #[error(transparent)]
    Rpc(RpcError),
    /// Signing failed (gate trip, malformed payload, or backend error).
    #[error(transparent)]
    Signer(SignerError),
    /// Policy denied the intent — carries the exact rule + offending field.
    #[error(transparent)]
    Policy(PolicyRejection),
    /// The policy engine failed operationally (load/eval), distinct from a denial.
    #[error(transparent)]
    PolicyEngine(PolicyEngineError),
    /// Fee estimation or bumping failed.
    #[error(transparent)]
    Gas(GasOracleError),
    /// Nonce allocation or reconciliation failed.
    #[error(transparent)]
    Nonce(NonceManagerError),
    /// Broadcasting the transaction failed.
    #[error(transparent)]
    Submission(SubmissionError),
    /// A durable-store operation failed.
    #[error(transparent)]
    Store(StateStoreError),
    /// A chain read failed (RPC transport or on-chain decode).
    #[error(transparent)]
    Read(ReadError),
    /// An ENS resolution failed (transport, offchain-required, or resolution error).
    #[error(transparent)]
    Ens(EnsError),
    /// An account-management operation failed (bad phrase/path, derivation, RNG, or a
    /// discovery read).
    #[error(transparent)]
    Account(AccountError),
    /// Pre-send simulation rejected the intent (it would revert).
    #[error("simulation rejected: {reason}")]
    Simulation {
        /// The decoded revert/rejection reason.
        reason: String,
    },
    /// The signer's address does not control the intent's account.
    #[error("signer {signer} does not control the intent account {intent}")]
    AccountMismatch {
        /// The intent's declared account.
        intent: Address,
        /// The signer's actual address.
        signer: Address,
    },
    /// A cancel could not proceed: the handle is unknown or already settled.
    #[error("cannot cancel: {reason}")]
    Cancel {
        /// Why the cancel could not proceed.
        reason: &'static str,
    },
    /// A convenience-constructor setup failure: a malformed RPC URL or a transport that
    /// could not be built.
    #[error("connection setup failed: {0}")]
    Connect(String),
    /// A submission route was invalid for its relay (e.g. Flashbots-only options on a
    /// generic Protect relay).
    #[error(transparent)]
    Route(RouteError),
    /// A gasless meta-transaction relay operation failed (forwarder read, request signing,
    /// self-relay broadcast, or a managed relay declining the request).
    #[error(transparent)]
    Relay(RelayError),
}

impl WalletKitError {
    /// The retry classification a caller should branch on.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Rpc(e) => rpc_kind(e),
            Self::Gas(GasOracleError::Rpc(e)) => rpc_kind(e),
            Self::Gas(GasOracleError::CeilingExceeded { .. }) => ErrorKind::Terminal,
            Self::Nonce(NonceManagerError::Rpc(e)) => rpc_kind(e),
            Self::Nonce(NonceManagerError::Store(e)) => store_kind(e),
            Self::Submission(e) => submission_kind(e),
            Self::Store(e) => store_kind(e),
            Self::Read(ReadError::Rpc(e)) => rpc_kind(e),
            Self::Read(ReadError::Decode { .. }) => ErrorKind::Terminal,
            Self::Ens(EnsError::Rpc(e)) => rpc_kind(e),
            Self::Ens(EnsError::OffchainLookupRequired | EnsError::Resolution { .. }) => {
                ErrorKind::Terminal
            }
            Self::Account(e) => account_kind(e),
            Self::Relay(e) => relay_kind(e),
            Self::Signer(_)
            | Self::Policy(_)
            | Self::PolicyEngine(_)
            | Self::Simulation { .. }
            | Self::AccountMismatch { .. }
            | Self::Cancel { .. }
            | Self::Route(_)
            | Self::Connect(_) => ErrorKind::Terminal,
        }
    }

    /// Whether an immediate retry of the same request is worthwhile.
    pub fn is_retryable(&self) -> bool {
        self.kind() == ErrorKind::Retryable
    }

    /// A suggested minimum backoff. `None` until the Transport surfaces server
    /// `Retry-After`/rate-limit hints; until then [`is_retryable`](Self::is_retryable) is
    /// the signal and the host paces retries.
    pub fn retry_after(&self) -> Option<Duration> {
        None
    }

    /// A short operator hint, when one is more useful than the error message alone.
    pub fn remediation(&self) -> Option<&'static str> {
        match self {
            Self::AccountMismatch { .. } => {
                Some("sign with the key that controls the intent account")
            }
            Self::Gas(GasOracleError::CeilingExceeded { .. }) => {
                Some("raise gas_ceiling or wait for the base fee to fall")
            }
            Self::Signer(
                SignerError::ApprovalExpired
                | SignerError::ApprovalMismatch
                | SignerError::FeesExceedApproval,
            ) => Some("re-submit the intent to obtain a fresh policy approval"),
            Self::Simulation { .. } => {
                Some("the transaction would revert — inspect calldata and account state")
            }
            Self::Cancel { .. } => {
                Some("the transaction already settled or was never tracked — nothing to cancel")
            }
            Self::Relay(RelayError::Forwarder { .. }) => Some(
                "check the forwarder address and that the target contract trusts it (ERC-2771)",
            ),
            Self::Relay(RelayError::NotConfigured) => Some(
                "set a relayer signer and forwarder via WalletBuilder::relayer(..).forwarder(..)",
            ),
            _ => None,
        }
    }

    /// The structured rejection when policy denied the intent.
    pub fn policy_rejection(&self) -> Option<&PolicyRejection> {
        match self {
            Self::Policy(r) => Some(r),
            _ => None,
        }
    }
}

fn rpc_kind(e: &RpcError) -> ErrorKind {
    match e {
        RpcError::Call {
            transient: true, ..
        } => ErrorKind::Retryable,
        RpcError::Call {
            transient: false, ..
        } => ErrorKind::Terminal,
    }
}

fn submission_kind(e: &SubmissionError) -> ErrorKind {
    if e.is_already_accepted() {
        ErrorKind::NeedsReconcile
    } else if e.is_transient() || e.is_underpriced() {
        ErrorKind::Retryable
    } else {
        ErrorKind::Terminal
    }
}

fn relay_kind(e: &RelayError) -> ErrorKind {
    // Delegate to the inner classifiers; a config/decline error is terminal.
    match e {
        RelayError::Rpc(e) => rpc_kind(e),
        RelayError::Submission(e) => submission_kind(e),
        RelayError::Signing(_)
        | RelayError::Rejected { .. }
        | RelayError::Forwarder { .. }
        | RelayError::NotConfigured => ErrorKind::Terminal,
    }
}

fn account_kind(e: &AccountError) -> ErrorKind {
    // A discovery read/RPC failure follows the transport's retry classification; a bad
    // phrase/path/derivation/RNG failure is terminal.
    match e {
        AccountError::Rpc(e) | AccountError::Read(ReadError::Rpc(e)) => rpc_kind(e),
        _ => ErrorKind::Terminal,
    }
}

fn store_kind(e: &StateStoreError) -> ErrorKind {
    match e {
        StateStoreError::Backend { .. } | StateStoreError::Task(_) => ErrorKind::Retryable,
        StateStoreError::Serialization { .. } | StateStoreError::Fenced => ErrorKind::Terminal,
    }
}

impl From<TransactionManagerError> for WalletKitError {
    fn from(e: TransactionManagerError) -> Self {
        use TransactionManagerError as E;
        match e {
            E::AccountMismatch { intent, signer } => Self::AccountMismatch { intent, signer },
            E::SimulationRejected { reason } => Self::Simulation { reason },
            E::UnknownHandle => Self::Cancel {
                reason: "no tracked transaction for this handle id",
            },
            E::CancelTerminal => Self::Cancel {
                reason: "the transaction already settled",
            },
            E::Denied(r) => Self::Policy(r),
            E::Rpc(e) => Self::Rpc(e),
            E::Gas(e) => Self::Gas(e),
            E::Policy(e) => Self::PolicyEngine(e),
            E::Nonce(e) => Self::Nonce(e),
            E::Signer(e) => Self::Signer(e),
            E::Store(e) => Self::Store(e),
            E::Submission(e) => Self::Submission(e),
            E::Route(e) => Self::Route(e),
        }
    }
}

impl From<ExecutorError> for WalletKitError {
    fn from(e: ExecutorError) -> Self {
        use ExecutorError as E;
        match e {
            E::Rpc(e) => Self::Rpc(e),
            E::Gas(e) => Self::Gas(e),
            E::Policy(e) => Self::PolicyEngine(e),
            E::Nonce(e) => Self::Nonce(e),
            E::Signer(e) => Self::Signer(e),
            E::Store(e) => Self::Store(e),
            E::Submission(e) => Self::Submission(e),
        }
    }
}

impl From<StateStoreError> for WalletKitError {
    fn from(e: StateStoreError) -> Self {
        Self::Store(e)
    }
}

impl From<ReadError> for WalletKitError {
    fn from(e: ReadError) -> Self {
        Self::Read(e)
    }
}

impl From<EnsError> for WalletKitError {
    fn from(e: EnsError) -> Self {
        Self::Ens(e)
    }
}

impl From<AccountError> for WalletKitError {
    fn from(e: AccountError) -> Self {
        Self::Account(e)
    }
}

impl From<RelayError> for WalletKitError {
    fn from(e: RelayError) -> Self {
        Self::Relay(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_rpc_is_retryable_with_no_backoff_yet() {
        let e = WalletKitError::Rpc(RpcError::Call {
            message: "timeout".into(),
            transient: true,
        });
        assert_eq!(e.kind(), ErrorKind::Retryable);
        assert!(e.is_retryable());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn non_transient_rpc_is_terminal() {
        let e = WalletKitError::Rpc(RpcError::Call {
            message: "method not found".into(),
            transient: false,
        });
        assert_eq!(e.kind(), ErrorKind::Terminal);
        assert!(!e.is_retryable());
    }

    #[test]
    fn already_known_submission_needs_reconcile() {
        // "already known" is a canonical already-accepted message -> reconcile, don't fail.
        let e = WalletKitError::Submission(SubmissionError::Rpc(RpcError::Call {
            message: "already known".into(),
            transient: false,
        }));
        assert_eq!(e.kind(), ErrorKind::NeedsReconcile);
    }

    #[test]
    fn ceiling_exceeded_is_terminal_with_remediation() {
        let e = WalletKitError::Gas(GasOracleError::CeilingExceeded {
            ceiling: 100,
            needed: 200,
        });
        assert_eq!(e.kind(), ErrorKind::Terminal);
        assert!(e.remediation().is_some());
    }

    #[test]
    fn denial_exposes_the_structured_rejection() {
        let rejection = PolicyRejection {
            rule: "spend_limit".into(),
            field: Some("value".into()),
            reason: "exceeds cap".into(),
        };
        let e = WalletKitError::from(TransactionManagerError::Denied(rejection));
        assert_eq!(e.kind(), ErrorKind::Terminal);
        let r = e.policy_rejection().expect("policy rejection present");
        assert_eq!(r.rule, "spend_limit");
        assert_eq!(r.field.as_deref(), Some("value"));
    }

    #[test]
    fn account_errors_classify_by_cause() {
        // A bad phrase is terminal; a discovery RPC failure follows the transport class.
        assert_eq!(
            WalletKitError::Account(AccountError::InvalidPhrase).kind(),
            ErrorKind::Terminal
        );
        assert_eq!(
            WalletKitError::Account(AccountError::Rpc(RpcError::Call {
                message: "timeout".into(),
                transient: true,
            }))
            .kind(),
            ErrorKind::Retryable
        );
    }

    #[test]
    fn relay_errors_classify_by_cause_and_hint_on_config() {
        // Inherits the inner transport class (delegates to rpc_kind)...
        assert_eq!(
            WalletKitError::Relay(RelayError::Rpc(RpcError::Call {
                message: "timeout".into(),
                transient: true,
            }))
            .kind(),
            ErrorKind::Retryable
        );
        // ...while a forwarder-config error is terminal with an actionable hint.
        let e = WalletKitError::Relay(RelayError::Forwarder {
            message: "target does not trust the forwarder".into(),
        });
        assert_eq!(e.kind(), ErrorKind::Terminal);
        assert!(e.remediation().is_some());
    }

    #[test]
    fn from_txmgr_flattens_domain_variants() {
        let acct = Address::ZERO;
        let e = WalletKitError::from(TransactionManagerError::AccountMismatch {
            intent: acct,
            signer: acct,
        });
        assert!(matches!(e, WalletKitError::AccountMismatch { .. }));
        let e = WalletKitError::from(TransactionManagerError::SimulationRejected {
            reason: "revert".into(),
        });
        assert!(matches!(e, WalletKitError::Simulation { .. }));
    }
}
