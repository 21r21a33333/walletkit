//! `ChainlinkPrice` — token prices from Chainlink `AggregatorV3Interface` feeds (pure RPC).
//! Staleness is checked **per feed** (heartbeats vary widely — ETH/USD is 3600s on mainnet
//! but 86400s on Arbitrum), against an injected [`Clock`] (no ambient time). Every round is
//! validated (positive answer, sane timestamps) before it becomes a `Price`.

use crate::adapters::multicall::contract_error;
use crate::core::deps::{Clock, Currency, Price, PriceSource, PricingError};
use alloy_contract::Error as ContractError;
use alloy_primitives::{Address, I256, U256};
use alloy_provider::DynProvider;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

alloy_sol_types::sol! {
    #[sol(rpc)]
    interface AggregatorV3Interface {
        function decimals() external view returns (uint8);
        function latestRoundData() external view returns (
            uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound
        );
    }
}

/// A Chainlink feed: its aggregator address and heartbeat (the max seconds between on-chain
/// updates for this feed on this chain).
pub struct FeedConfig {
    pub address: Address,
    pub heartbeat_secs: u64,
}

/// Prices tokens from a `(chain_id, token) -> FeedConfig` map. A missing feed is `Ok(None)`;
/// a stale/invalid round is an `Err`.
pub struct ChainlinkPrice {
    provider: DynProvider,
    clock: Arc<dyn Clock>,
    feeds: HashMap<(u64, Address), FeedConfig>,
    grace_secs: u64,
}

impl ChainlinkPrice {
    pub fn new(
        provider: DynProvider,
        clock: Arc<dyn Clock>,
        feeds: HashMap<(u64, Address), FeedConfig>,
        grace_secs: u64,
    ) -> Self {
        Self {
            provider,
            clock,
            feeds,
            grace_secs,
        }
    }
}

/// One decoded `latestRoundData` round, reduced to what pricing needs.
struct Round {
    answer: I256,
    updated_at: u64,
}

#[async_trait]
impl PriceSource for ChainlinkPrice {
    async fn price(
        &self,
        chain_id: u64,
        token: Address,
        vs: Currency,
    ) -> Result<Option<Price>, PricingError> {
        // Only USD feeds are configured today; other quote currencies have no feed.
        match vs {
            Currency::Usd => {}
        }
        let Some(feed) = self.feeds.get(&(chain_id, token)) else {
            return Ok(None);
        };
        let agg = AggregatorV3Interface::new(feed.address, &self.provider);
        let decimals = agg.decimals().call().await.map_err(price_err)?;
        let data = agg.latestRoundData().call().await.map_err(price_err)?;
        if data.roundId.is_zero() {
            return Err(PricingError::Feed {
                detail: "zero round id".into(),
            });
        }
        let round = Round {
            answer: data.answer,
            updated_at: data.updatedAt.saturating_to::<u64>(),
        };
        let max_age = feed.heartbeat_secs.saturating_add(self.grace_secs);
        to_price(round, decimals, self.clock.now_unix(), max_age).map(Some)
    }
}

/// Turn a Chainlink round into a `Price`, or reject it. Pure: no I/O. Enforces the feed's own
/// staleness window (`heartbeat + grace`, passed as `max_age`), not a single global constant.
fn to_price(
    round: Round,
    decimals: u8,
    now: u64,
    max_age_secs: u64,
) -> Result<Price, PricingError> {
    if round.updated_at == 0 || round.updated_at > now {
        return Err(PricingError::Feed {
            detail: "invalid round timestamp".into(),
        });
    }
    // A non-positive answer is a feed fault (stale/misconfigured), not a real price.
    if round.answer <= I256::ZERO {
        return Err(PricingError::Feed {
            detail: "non-positive price".into(),
        });
    }
    let age = now - round.updated_at;
    if age > max_age_secs {
        return Err(PricingError::Stale { age_secs: age });
    }
    let value = U256::try_from(round.answer).map_err(|_| PricingError::Feed {
        detail: "price overflow".into(),
    })?;
    Ok(Price {
        value,
        decimals,
        updated_at: round.updated_at,
    })
}

/// A feed read failed: a transport failure keeps its transient classification; a revert or
/// decode fault is terminal.
fn price_err(e: ContractError) -> PricingError {
    PricingError::Rpc(contract_error(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(answer: i128, updated_at: u64) -> Round {
        Round {
            answer: I256::try_from(answer).unwrap(),
            updated_at,
        }
    }

    #[test]
    fn fresh_round_scales_and_stale_or_bad_rounds_reject() {
        // Fresh: age 30s within the feed's max_age (3600 heartbeat + 60 grace).
        let p = to_price(round(200_000_000_000, 1_000), 8, 1_030, 3_660).unwrap();
        assert_eq!(
            (p.value, p.decimals, p.updated_at),
            (U256::from(200_000_000_000u64), 8, 1_000)
        );

        // Stale: age 4000s exceeds max_age.
        assert!(matches!(
            to_price(round(1, 1_000), 8, 5_000, 3_660),
            Err(PricingError::Stale { age_secs: 4000 })
        ));

        // Non-positive answer and future/zero timestamp are feed faults.
        assert!(matches!(
            to_price(round(0, 1_000), 8, 1_030, 3_660),
            Err(PricingError::Feed { .. })
        ));
        assert!(matches!(
            to_price(round(1, 0), 8, 1_030, 3_660),
            Err(PricingError::Feed { .. })
        ));
    }
}
