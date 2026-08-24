//! Opt-in pricing seam (feature `pricing`): off-chain/vendor-neutral token metadata and
//! token prices, independent of the on-chain [`ReadClient`](crate::core::deps::ReadClient).
//! Core never calls these; a caller composes them in. RPC-compatible adapters (token-list,
//! Chainlink) ship in-crate; vendor HTTP adapters (CoinGecko, …) are the same ports,
//! deferred.

use crate::core::deps::{Erc20Metadata, RpcError};
use alloy_primitives::{Address, U256};
use async_trait::async_trait;

/// A source of token display metadata (e.g. a Uniswap token-list), keyed by chain + address.
#[async_trait]
pub trait TokenMetadataSource: Send + Sync {
    async fn metadata(
        &self,
        chain_id: u64,
        token: Address,
    ) -> Result<Option<Erc20Metadata>, PricingError>;
}

/// A price feed for a token in a quote currency.
#[async_trait]
pub trait PriceSource: Send + Sync {
    async fn price(
        &self,
        chain_id: u64,
        token: Address,
        vs: Currency,
    ) -> Result<Option<Price>, PricingError>;
}

/// A token price as `value` scaled by `decimals` (e.g. Chainlink's feed decimals), with the
/// feed's `updated_at` (unix seconds) so a caller can reason about freshness.
#[non_exhaustive]
pub struct Price {
    pub value: U256,
    pub decimals: u8,
    pub updated_at: u64,
}

/// The quote currency of a [`Price`].
#[non_exhaustive]
pub enum Currency {
    Usd,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PricingError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("token list error: {detail}")]
    List { detail: String },
    /// A price-feed round failed validation (non-positive answer, zero/future timestamp, …).
    #[error("invalid price feed round: {detail}")]
    Feed { detail: String },
    #[error("price feed stale by {age_secs}s")]
    Stale { age_secs: u64 },
}
