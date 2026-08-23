//! The public error taxonomy (SPEC §5.5). Per-port `{Trait}Error`s remain the internal
//! contracts; `WalletKitError` is the one umbrella every `Wallet` operation returns,
//! classified for retry with a machine-readable [`ErrorKind`].

use crate::core::deps::{
    GasOracleError, NonceManagerError, PolicyEngineError, RpcError, SignerError, StateStoreError,
    SubmissionError,
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
    #[error(transparent)]
    Rpc(RpcError),
    #[error(transparent)]
    Signer(SignerError),
    /// Policy denied the intent — carries the exact rule + offending field.
    #[error(transparent)]
    Policy(PolicyRejection),
    /// The policy engine failed operationally (load/eval), distinct from a denial.
    #[error(transparent)]
    PolicyEngine(PolicyEngineError),
    #[error(transparent)]
    Gas(GasOracleError),
    #[error(transparent)]
    Nonce(NonceManagerError),
    #[error(transparent)]
    Submission(SubmissionError),
    #[error(transparent)]
    Store(StateStoreError),
    #[error("simulation rejected: {reason}")]
    Simulation { reason: String },
    #[error("signer {signer} does not control the intent account {intent}")]
    AccountMismatch { intent: Address, signer: Address },
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
            Self::Signer(_)
            | Self::Policy(_)
            | Self::PolicyEngine(_)
            | Self::Simulation { .. }
            | Self::AccountMismatch { .. } => ErrorKind::Terminal,
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
    } else if e.is_transient() {
        ErrorKind::Retryable
    } else {
        ErrorKind::Terminal
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
            E::Denied(r) => Self::Policy(r),
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
