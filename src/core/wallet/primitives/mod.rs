//! Wallet domain primitives — the intent/approval types ([`intent`]) and the
//! policy verdict contract ([`policy`]). All domain structs live here.

mod intent;
mod policy;

pub use intent::{IntentHash, TxIntent};
pub use policy::{Decision, PolicyApproval, PolicyRejection};
