//! Wallet domain types. All primitives live under [`primitives`]; this module is
//! a thin re-export surface.

mod executor;
mod primitives;
mod transaction_manager;

pub use executor::AccountExecutor;
pub use primitives::{
    Decision, GasEnvelope, HandleId, IntentHash, NonceLane, NonceScope, NonceState, PolicyApproval,
    PolicyRejection, TxHandle, TxIntent, TxStatus,
};
pub use transaction_manager::{TransactionManager, TransactionManagerError};
