//! Wallet domain types. All primitives live under [`primitives`]; this module is
//! a thin re-export surface.

mod primitives;

pub use primitives::{
    Decision, IntentHash, NonceLane, NonceScope, NonceState, PolicyApproval, PolicyRejection,
    TxIntent,
};
