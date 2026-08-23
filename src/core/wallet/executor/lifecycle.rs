//! The transaction lifecycle state machine — a **pure** `transition(state, event)`
//! with no I/O, so every rule is exhaustively table-testable without mocks
//! (functional core / imperative shell; the shell is [`AccountExecutor`]).
//!
//! The states are [`TxStatus`] itself — the FSM states *are* the persisted
//! statuses, so there is no parallel type to keep in sync. The shell distills each
//! unreliable chain read into one trustworthy [`ChainEvent`] (hash-anchored) and
//! this function decides the next status. The core safety property: an ambiguous read
//! arrives as [`ChainEvent::Unknown`], which is **never** a transition — a bad read can
//! neither advance nor rewind the lifecycle.
//!
//! [`AccountExecutor`]: super::AccountExecutor

use crate::core::wallet::TxStatus;
use alloy_primitives::B256;

/// How an outcome becomes irreversible. The shell picks the mode per cycle: prefer
/// the `finalized` tag, fall back to a depth count when a chain doesn't expose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    /// Terminal once the outcome's block is at or below the `finalized` head.
    Finalized,
    /// Terminal once `latest - block + 1 >= required` confirmations (no finalized tag).
    Depth,
}

/// The finality rule for a cycle: the mode plus the depth used in [`Finality::Depth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinalityConfig {
    pub mode: Finality,
    pub required: u64,
}

/// The chain's finality context for one cycle. `finalized` is meaningful only under
/// [`Finality::Finalized`]; `latest` drives the depth count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainView {
    pub latest: u64,
    pub finalized: u64,
}

/// How a mined transaction executed (the EIP-658 receipt status; viem's
/// `success`/`reverted`). A reverted tx is still mined — it consumed its nonce and
/// gas — so it settles as `Failed`, not a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Executed,
    Reverted,
}

/// A trustworthy, distilled read for one handle this cycle — the shell produces
/// exactly one, collapsing every unreliable-read case into [`Unknown`](ChainEvent::Unknown).
/// Variant names follow the ecosystem's tx-lifecycle vocabulary (reth
/// `TransactionEvent`, ethers/viem `TRANSACTION_REPLACED`, OZ Relayer / Alchemy
/// statuses): `Mined` is our own tx, `Replaced` is a foreign one at our nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainEvent {
    /// Our nonce is still open on-chain — not yet mined (in the mempool).
    Pending,
    /// One of our broadcasts is canonically mined here (receipt hash-anchored to
    /// `block`). A re-mine in a new block simply arrives as a fresh `Mined`.
    Mined {
        block: u64,
        block_hash: B256,
        outcome: Outcome,
    },
    /// A foreign transaction consumed our nonce — always a different sender's tx,
    /// since our own bumps mine as `Mined`.
    Replaced,
    /// Stale, inconsistent, or a gap — no evidence to act on. No industry term names
    /// this; it is our read-robustness no-op.
    Unknown,
}

/// The next status for one handle given a distilled chain event, or `None` for no
/// change (unchanged, tentative, or [`Unknown`](ChainEvent::Unknown)).
///
/// Terminal statuses are finalized and never revised (a `Confirmed` never
/// un-confirms). Shallower outcomes stay tentative so a reorg — surfaced as
/// `Pending` (nonce freed) or `Unknown` (bad read) — can still recover them.
pub fn transition(
    state: &TxStatus,
    event: &ChainEvent,
    view: &ChainView,
    cfg: &FinalityConfig,
) -> Option<TxStatus> {
    // A finalized outcome is cryptoeconomically irreversible — no event revises it.
    if state.is_terminal() {
        return None;
    }
    let target = match event {
        ChainEvent::Unknown => return None,
        // Nonce still open: a tentative Mined/Replacing was un-mined by a reorg that
        // freed the nonce, so re-track from Sent; an un-mined nonce is otherwise a no-op.
        ChainEvent::Pending => match state {
            TxStatus::Mined { .. } | TxStatus::Replacing { .. } => TxStatus::Sent,
            _ => return None,
        },
        ChainEvent::Mined {
            block,
            block_hash,
            outcome,
        } => {
            if is_final(*block, view, cfg) {
                match outcome {
                    Outcome::Executed => TxStatus::Confirmed { block: *block },
                    Outcome::Reverted => TxStatus::Failed {
                        reason: "reverted on-chain".into(),
                    },
                }
            } else {
                TxStatus::Mined {
                    block: *block,
                    block_hash: *block_hash,
                }
            }
        }
        // A foreign tx holds our nonce. Depth-gate from when we first saw it so a reorg
        // that frees the nonce (a later Pending) can still recover our tx.
        ChainEvent::Replaced => match state {
            TxStatus::Replacing { since_block } if is_final(*since_block, view, cfg) => {
                TxStatus::Replaced
            }
            TxStatus::Replacing { .. } => return None,
            _ => TxStatus::Replacing {
                since_block: view.latest,
            },
        },
    };
    (target != *state).then_some(target)
}

/// Whether an outcome at `block` is irreversible under the cycle's finality rule.
/// Depth clamps to at least one confirmation so a lagging `latest` never underflows.
fn is_final(block: u64, view: &ChainView, cfg: &FinalityConfig) -> bool {
    match cfg.mode {
        Finality::Finalized => block <= view.finalized,
        Finality::Depth => view.latest.saturating_sub(block) + 1 >= cfg.required,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: B256 = B256::ZERO;

    fn depth(required: u64) -> FinalityConfig {
        FinalityConfig {
            mode: Finality::Depth,
            required,
        }
    }
    fn finalized() -> FinalityConfig {
        FinalityConfig {
            mode: Finality::Finalized,
            required: 0,
        }
    }
    fn view(latest: u64, finalized: u64) -> ChainView {
        ChainView { latest, finalized }
    }
    fn mined(block: u64, outcome: Outcome) -> ChainEvent {
        ChainEvent::Mined {
            block,
            block_hash: HASH,
            outcome,
        }
    }

    #[test]
    fn unknown_never_transitions() {
        // The crux: a bad read must not move any state.
        for state in [
            TxStatus::Sent,
            TxStatus::Mined {
                block: 8,
                block_hash: HASH,
            },
            TxStatus::Replacing { since_block: 8 },
        ] {
            assert_eq!(
                transition(&state, &ChainEvent::Unknown, &view(100, 100), &depth(1)),
                None
            );
        }
    }

    #[test]
    fn terminal_states_are_never_revised() {
        // A Confirmed never un-confirms, even on a contradicting event (I2).
        for state in [
            TxStatus::Confirmed { block: 8 },
            TxStatus::Failed { reason: "x".into() },
            TxStatus::Replaced,
        ] {
            assert_eq!(
                transition(
                    &state,
                    &mined(8, Outcome::Reverted),
                    &view(100, 100),
                    &depth(1)
                ),
                None
            );
            assert_eq!(
                transition(&state, &ChainEvent::Pending, &view(100, 100), &depth(1)),
                None
            );
        }
    }

    #[test]
    fn pending_reorgs_tentative_states_back_to_sent() {
        let cfg = depth(2);
        let v = view(10, 0);
        // A freed nonce un-mines a tentative Mined/Replacing.
        assert_eq!(
            transition(
                &TxStatus::Mined {
                    block: 8,
                    block_hash: HASH
                },
                &ChainEvent::Pending,
                &v,
                &cfg
            ),
            Some(TxStatus::Sent)
        );
        assert_eq!(
            transition(
                &TxStatus::Replacing { since_block: 8 },
                &ChainEvent::Pending,
                &v,
                &cfg
            ),
            Some(TxStatus::Sent)
        );
        // A still-pending nonce is simply not there yet.
        assert_eq!(
            transition(&TxStatus::Sent, &ChainEvent::Pending, &v, &cfg),
            None
        );
    }

    #[test]
    fn mined_confirms_and_fails_only_at_depth() {
        let cfg = depth(2); // need 2 confirmations
        // block 8, latest 8 -> depth 1 < 2: tentative Mined, not terminal.
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Executed),
                &view(8, 0),
                &cfg
            ),
            Some(TxStatus::Mined {
                block: 8,
                block_hash: HASH
            })
        );
        // latest 9 -> depth 2 >= 2: success confirms, revert fails.
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Executed),
                &view(9, 0),
                &cfg
            ),
            Some(TxStatus::Confirmed { block: 8 })
        );
        assert!(matches!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Reverted),
                &view(9, 0),
                &cfg
            ),
            Some(TxStatus::Failed { .. })
        ));
    }

    #[test]
    fn mined_at_same_block_is_a_no_op() {
        // Re-seeing the same tentative block must not churn a persist.
        let cfg = depth(5);
        let state = TxStatus::Mined {
            block: 8,
            block_hash: HASH,
        };
        assert_eq!(
            transition(&state, &mined(8, Outcome::Executed), &view(9, 0), &cfg),
            None
        );
    }

    #[test]
    fn depth_clamps_when_latest_lags_behind_the_block() {
        // head < block (node skew) must not underflow: it counts as 1 confirmation.
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(10, Outcome::Executed),
                &view(8, 0),
                &depth(1)
            ),
            Some(TxStatus::Confirmed { block: 10 })
        );
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(10, Outcome::Executed),
                &view(8, 0),
                &depth(2)
            ),
            Some(TxStatus::Mined {
                block: 10,
                block_hash: HASH
            })
        );
    }

    #[test]
    fn depth_counts_skipped_blocks() {
        // latest jumps 8 -> 13 (skipped/batched blocks) still crosses the threshold.
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Executed),
                &view(13, 0),
                &depth(3)
            ),
            Some(TxStatus::Confirmed { block: 8 })
        );
    }

    #[test]
    fn finalized_mode_gates_on_the_finalized_head() {
        // Terminal only when the block is at or below `finalized`, regardless of latest.
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Executed),
                &view(100, 7),
                &finalized()
            ),
            Some(TxStatus::Mined {
                block: 8,
                block_hash: HASH
            })
        );
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Executed),
                &view(100, 8),
                &finalized()
            ),
            Some(TxStatus::Confirmed { block: 8 })
        );
    }

    #[test]
    fn replaced_is_tentative_until_depth_then_final() {
        let cfg = depth(3);
        // First sight starts the depth clock at latest.
        assert_eq!(
            transition(&TxStatus::Sent, &ChainEvent::Replaced, &view(10, 0), &cfg),
            Some(TxStatus::Replacing { since_block: 10 })
        );
        // Not yet deep -> stay tentative.
        assert_eq!(
            transition(
                &TxStatus::Replacing { since_block: 10 },
                &ChainEvent::Replaced,
                &view(11, 0),
                &cfg
            ),
            None
        );
        // Deep enough -> terminal Replaced.
        assert_eq!(
            transition(
                &TxStatus::Replacing { since_block: 10 },
                &ChainEvent::Replaced,
                &view(12, 0),
                &cfg
            ),
            Some(TxStatus::Replaced)
        );
    }

    #[test]
    fn replaced_after_mined_restarts_the_depth_clock() {
        // Our mined tx lost its nonce to a foreign tx (reorg) -> Replacing, not stranded.
        assert_eq!(
            transition(
                &TxStatus::Mined {
                    block: 8,
                    block_hash: HASH
                },
                &ChainEvent::Replaced,
                &view(10, 0),
                &depth(3)
            ),
            Some(TxStatus::Replacing { since_block: 10 })
        );
    }

    #[test]
    fn zero_required_depth_confirms_at_the_inclusion_block() {
        // required == 0 (a zero-conf L2): terminal at the exact inclusion block. Pins the
        // `+1` clamp — latest == block gives saturating_sub(8)+1 = 1 >= 0.
        assert_eq!(
            transition(
                &TxStatus::Sent,
                &mined(8, Outcome::Executed),
                &view(8, 0),
                &depth(0)
            ),
            Some(TxStatus::Confirmed { block: 8 })
        );
    }

    #[test]
    fn replacing_is_reclaimed_by_our_own_mined_tx() {
        // A reorg re-included our tx after we'd marked the nonce Replacing. The Mined arm
        // does not gate on state, so Replacing -> tentative Mined -> Confirmed at depth;
        // fails the instant anyone restricts the Mined arm to Sent/Mined.
        let cfg = depth(2);
        assert_eq!(
            transition(
                &TxStatus::Replacing { since_block: 5 },
                &mined(8, Outcome::Executed),
                &view(8, 0),
                &cfg
            ),
            Some(TxStatus::Mined {
                block: 8,
                block_hash: HASH
            })
        );
        assert_eq!(
            transition(
                &TxStatus::Replacing { since_block: 5 },
                &mined(8, Outcome::Executed),
                &view(9, 0),
                &cfg
            ),
            Some(TxStatus::Confirmed { block: 8 })
        );
    }

    #[test]
    fn remine_into_a_different_block_updates_the_anchor() {
        // A reorg re-includes our tx at a *different* block: advance both block and hash
        // (re-keying the depth clock), never a no-op. Complements the same-block no-op test.
        let hash_a = B256::repeat_byte(0xa);
        let hash_b = B256::repeat_byte(0xb);
        assert_eq!(
            transition(
                &TxStatus::Mined {
                    block: 8,
                    block_hash: hash_a,
                },
                &ChainEvent::Mined {
                    block: 12,
                    block_hash: hash_b,
                    outcome: Outcome::Executed,
                },
                &view(12, 0),
                &depth(4)
            ),
            Some(TxStatus::Mined {
                block: 12,
                block_hash: hash_b,
            })
        );
    }
}
