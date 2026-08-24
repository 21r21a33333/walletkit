//! Pricing adapters (feature `pricing`): a Uniswap token-list
//! [`TokenMetadataSource`](crate::core::deps::TokenMetadataSource) and a Chainlink
//! [`PriceSource`](crate::core::deps::PriceSource), both RPC-compatible. Vendor HTTP adapters
//! (CoinGecko, …) are the same ports, deferred.

pub mod chainlink;
pub mod token_list;

pub use chainlink::{ChainlinkPrice, FeedConfig};
pub use token_list::TokenListSource;
