//! Wallet domain types. All primitives live under [`primitives`]; this module is
//! a thin re-export surface.

mod executor;
mod primitives;
mod signing;
mod transaction_manager;

pub use executor::{
    AccountExecutor, ChainEvent, ChainView, Finality, FinalityConfig, Outcome, transition,
};
pub use primitives::{
    Decision, GasEnvelope, HandleId, IntentHash, NonceScope, NonceState, PolicyApproval,
    PolicyRejection, TxHandle, TxIntent, TxStatus,
};
pub use transaction_manager::{TransactionManager, TransactionManagerError};
