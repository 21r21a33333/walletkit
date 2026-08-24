//! `EnsResolver` — object-safe ENS name resolution over plain RPC. `None` means "no
//! record" (unregistered, no resolver, or a reverse name that fails forward-verification);
//! only transport/operational failures are `Err`. Names are hashed verbatim, so the caller
//! passes ENSIP-15-normalized names.

use crate::core::deps::RpcError;
use alloy_primitives::Address;
use async_trait::async_trait;

#[async_trait]
pub trait EnsResolver: Send + Sync {
    /// Resolve a name to its address, or `None` if unset/unregistered.
    async fn resolve_name(&self, name: &str) -> Result<Option<Address>, EnsError>;
    /// The primary name for an address, **forward-verified** (the name must resolve back to
    /// the same address), or `None` if unset or unverified.
    async fn reverse_lookup(&self, address: Address) -> Result<Option<String>, EnsError>;
    /// A text record (`key`) for a name, or `None` if unset.
    async fn text_record(&self, name: &str, key: &str) -> Result<Option<String>, EnsError>;
    /// The `avatar` text record (an URL or an `eip155:` NFT reference); NFT resolution is a
    /// deferred, separate concern.
    async fn avatar(&self, name: &str) -> Result<Option<String>, EnsError> {
        self.text_record(name, "avatar").await
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnsError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// The name needs EIP-3668 CCIP-Read (an offchain/L2 name — Basenames `*.base.eth`,
    /// `*.cb.id`, L2 subnames). Strict RPC does not follow the gateway hop; surfaced
    /// distinctly so a caller can opt into a future CCIP feature.
    #[error("ens name requires offchain CCIP-Read resolution")]
    OffchainLookupRequired,
    /// An ENS-specific operational failure (bad resolver, malformed name). Distinct from
    /// "no record", which is `Ok(None)`.
    #[error("ens resolution failed: {detail}")]
    Resolution { detail: String },
}
