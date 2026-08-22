//! Wallet domain primitives — the intent/approval types ([`intent`]), the policy
//! verdict contract ([`policy`]), and the nonce model ([`nonce`]). All domain
//! structs live here.

mod intent;
mod nonce;
mod policy;

pub use intent::{IntentHash, TxIntent};
pub use nonce::{NonceLane, NonceScope, NonceState};
pub use policy::{Decision, PolicyApproval, PolicyRejection};
