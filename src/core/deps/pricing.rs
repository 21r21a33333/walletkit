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
    /// Metadata for `token` on `chain_id`, or `None` if the source doesn't list it.
    async fn metadata(
        &self,
        chain_id: u64,
        token: Address,
    ) -> Result<Option<Erc20Metadata>, PricingError>;
}

/// A price feed for a token in a quote currency.
#[async_trait]
pub trait PriceSource: Send + Sync {
    /// Price of `token` on `chain_id` in `vs`, or `None` if the source has no feed for it.
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
    /// The price, an integer scaled by `decimals`.
    pub value: U256,
    /// Decimal places `value` is scaled by (e.g. the feed's decimals).
    pub decimals: u8,
    /// Feed publish time, unix seconds — for freshness checks.
    pub updated_at: u64,
}

/// The quote currency of a [`Price`].
#[non_exhaustive]
pub enum Currency {
    /// US dollars.
    Usd,
}

/// Why a metadata/price lookup failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PricingError {
    /// An underlying RPC call failed (on-chain feeds).
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// A token-list source failed.
    #[error("token list error: {detail}")]
    List {
        /// What went wrong.
        detail: String,
    },
    /// A price-feed round failed validation (non-positive answer, zero/future timestamp, …).
    #[error("invalid price feed round: {detail}")]
    Feed {
        /// What failed validation.
        detail: String,
    },
    /// The latest feed round is older than the caller's staleness tolerance.
    #[error("price feed stale by {age_secs}s")]
    Stale {
        /// How far past the tolerance the round is, in seconds.
        age_secs: u64,
    },
}
