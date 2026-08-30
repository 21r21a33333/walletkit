//! Gasless meta-transaction primitives (ERC-2771) — the relayer-facing domain types grouped
//! in one place: the signed [`ForwardRequest`] and its [`ForwarderDomain`]. Pure (zero I/O);
//! the reads and adapters that use them live in the gasless service and `adapters/relay`.

mod forward_request;
mod meta_context;

pub use forward_request::{ForwardRequest, ForwarderDomain};
pub(crate) use forward_request::{decode_forwarder_nonce, execute_calldata, nonces_calldata};
pub use meta_context::MetaContext;
