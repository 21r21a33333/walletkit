//! Wallet domain types. All primitives live under `primitives`; this module is
//! a thin re-export surface.

mod executor;
mod preview;
mod primitives;
mod signing;
mod transaction_manager;

pub use executor::{
    AccountExecutor, ChainEvent, ChainView, ExecutorError, Finality, FinalityConfig, Outcome,
    transition,
};
pub(crate) use preview::dry_run;
pub use preview::{RevertReason, SimOutcome, TxPreview, decode_revert};
pub use primitives::{
    Decision, FenceToken, GasEnvelope, HandleId, IntentHash, NonceScope, NonceState,
    PolicyApproval, PolicyOutcome, PolicyRejection, SignatureEnvelope, SigningError,
    SigningRequest, SigningScheme, TxHandle, TxIntent, TxStatus, enforce_low_s, typed_data_hash,
};
pub use transaction_manager::{TransactionManager, TransactionManagerError};
