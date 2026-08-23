//! Wallet domain primitives — the intent/approval types ([`intent`]), the policy
//! verdict contract ([`policy`]), and the nonce model ([`nonce`]). All domain
//! structs live here.

mod handle;
mod intent;
mod nonce;
mod policy;

pub use handle::{HandleId, TxHandle, TxStatus};
pub use intent::{IntentHash, TxIntent};
pub use nonce::{NonceScope, NonceState};
pub use policy::{Decision, GasEnvelope, PolicyApproval, PolicyRejection};
