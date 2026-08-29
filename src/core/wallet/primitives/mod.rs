//! Wallet domain primitives — the intent/approval types ([`intent`]), the policy
//! verdict contract ([`policy`]), the nonce model ([`nonce`]), and the gasless
//! meta-transaction types ([`gasless`]). All domain structs live here.

mod gasless;
mod handle;
mod intent;
mod nonce;
mod policy;
mod signing_request;

pub use gasless::{ForwardRequest, ForwarderDomain, MetaContext};
pub use handle::{HandleId, TxHandle, TxStatus};
pub use intent::{IntentHash, TxIntent};
pub use nonce::{FenceToken, NonceScope, NonceState};
pub use policy::{Decision, GasEnvelope, PolicyApproval, PolicyOutcome, PolicyRejection};
pub use signing_request::{
    SignatureEnvelope, SigningError, SigningRequest, SigningScheme, enforce_low_s, typed_data_hash,
};
