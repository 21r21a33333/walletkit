//! `RpcGasOracle` — `estimate` delegates to alloy's default estimator; `bump`
//! applies geth's replacement rule plus alloy base-fee coverage. Constants are
//! cited at their definitions.

use crate::core::deps::{GasOracle, GasOracleError, Rpc};
use alloy_eips::eip1559::Eip1559Estimation;
use async_trait::async_trait;
use std::sync::Arc;

/// geth `DefaultConfig.PriceBump` — min replacement increase per fee field (percent).
const PRICE_BUMP_PCT: u128 = 10;
/// alloy `EIP1559_BASE_FEE_MULTIPLIER`: `maxFee = 2·baseFee + tip` (~6 blocks headroom).
const BASE_FEE_MULTIPLIER: u128 = 2;

/// Prices EIP-1559 fees over a chain-bound [`Rpc`]; won't bump past `ceiling_max_fee`.
pub struct RpcGasOracle {
    rpc: Arc<dyn Rpc>,
    ceiling_max_fee: u128,
}

impl RpcGasOracle {
    pub fn new(rpc: Arc<dyn Rpc>, ceiling_max_fee: u128) -> Self {
        Self {
            rpc,
            ceiling_max_fee,
        }
    }
}

#[async_trait]
impl GasOracle for RpcGasOracle {
    async fn estimate(&self) -> Result<Eip1559Estimation, GasOracleError> {
        Ok(self.rpc.estimate_fees().await?)
    }

    async fn bump(&self, prev: Eip1559Estimation) -> Result<Eip1559Estimation, GasOracleError> {
        // geth `legacypool/list.go` admits a replacement at `new >= floor((100+bump)·old/100)`;
        // ceil + max(old+1) clears that and forces a real increase (geth's floor allows wei-level
        // equality). Integer math to stay big.Int-exact.
        let rbf = |old: u128| {
            old.saturating_mul(100 + PRICE_BUMP_PCT)
                .div_ceil(100)
                .max(old + 1)
        };
        let tip = rbf(prev.max_priority_fee_per_gas);
        let rbf_cap = rbf(prev.max_fee_per_gas);

        // Cover base-fee growth (alloy's 2× rule) so the replacement stays includable.
        let base_fee = self.rpc.base_fee().await?;
        let coverage = base_fee
            .saturating_mul(BASE_FEE_MULTIPLIER)
            .saturating_add(tip);
        let max_fee = rbf_cap.max(coverage);

        if max_fee > self.ceiling_max_fee {
            return Err(GasOracleError::CeilingExceeded {
                ceiling: self.ceiling_max_fee,
                needed: max_fee,
            });
        }
        Ok(Eip1559Estimation {
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: tip,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::RpcError;
    use alloy_primitives::{Address, Bytes, TxHash};
    use alloy_rpc_types_eth::TransactionReceipt;

    /// Only `base_fee` is exercised by `bump`.
    struct FakeRpc {
        base_fee: u128,
    }

    #[async_trait]
    impl Rpc for FakeRpc {
        async fn base_fee(&self) -> Result<u128, RpcError> {
            Ok(self.base_fee)
        }
        async fn pending_nonce(&self, _: Address) -> Result<u64, RpcError> {
            unreachable!()
        }
        async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
            unreachable!()
        }
        async fn send_raw(&self, _: Bytes) -> Result<TxHash, RpcError> {
            unreachable!()
        }
        async fn receipt(&self, _: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
            unreachable!()
        }
    }

    fn oracle(base_fee: u128, ceiling: u128) -> RpcGasOracle {
        RpcGasOracle::new(Arc::new(FakeRpc { base_fee }), ceiling)
    }

    fn prev(cap: u128, tip: u128) -> Eip1559Estimation {
        Eip1559Estimation {
            max_fee_per_gas: cap,
            max_priority_fee_per_gas: tip,
        }
    }

    #[tokio::test]
    async fn bump_meets_geth_threshold_and_strict_greater_on_both_fields() {
        // Low base fee so the RBF cap, not coverage, sets max_fee.
        let next = oracle(1, u128::MAX).bump(prev(1_000, 100)).await.unwrap();
        assert_eq!(next.max_priority_fee_per_gas, 110); // ceil(1.1 * 100)
        assert_eq!(next.max_fee_per_gas, 1_100); // ceil(1.1 * 1000)
        assert!(next.max_fee_per_gas > 1_000 && next.max_priority_fee_per_gas > 100);
    }

    #[tokio::test]
    async fn bump_low_wei_still_strictly_increases() {
        // geth's floor admits floor(1.1*1)=1 (equality); max(old+1) forces 2.
        let next = oracle(0, u128::MAX).bump(prev(1, 1)).await.unwrap();
        assert_eq!(next.max_priority_fee_per_gas, 2);
        assert_eq!(next.max_fee_per_gas, 2);
    }

    #[tokio::test]
    async fn bump_covers_base_fee_via_2x_multiplier() {
        // High base fee — coverage (2*baseFee + tip) dominates the RBF cap.
        let next = oracle(1_000_000, u128::MAX)
            .bump(prev(1_000, 100))
            .await
            .unwrap();
        assert_eq!(next.max_priority_fee_per_gas, 110);
        assert_eq!(next.max_fee_per_gas, 2_000_110); // 2 * 1_000_000 + 110
        assert!(next.max_fee_per_gas >= 1_100); // still clears the RBF cap
    }

    #[tokio::test]
    async fn bump_errors_at_ceiling_instead_of_looping() {
        let err = oracle(1, 1_050).bump(prev(1_000, 100)).await.unwrap_err();
        assert!(matches!(
            err,
            GasOracleError::CeilingExceeded {
                ceiling: 1_050,
                needed: 1_100
            }
        ));
    }
}
