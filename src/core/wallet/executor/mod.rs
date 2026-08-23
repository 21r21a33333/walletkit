//! `AccountExecutor` — the per-account tracking loop that turns broadcast txs into
//! terminal outcomes. Separate from the one-shot send pipeline
//! ([`TransactionManager`](super::TransactionManager)) because it is a distinct
//! concern: the send path builds and submits once, the executor tracks forever.
//!
//! The pure state machine it drives lives in [`lifecycle`]; this module is its
//! imperative shell.

mod lifecycle;

pub use lifecycle::{ChainEvent, ChainView, Finality, FinalityConfig, Outcome, transition};

use super::signing;
use crate::core::deps::{
    Clock, GasOracle, GasOracleError, NonceManager, NonceManagerError, PolicyEngine,
    PolicyEngineError, Rpc, RpcError, Signer, SignerError, StateStore, StateStoreError,
    SubmissionError, SubmissionStrategy,
};
use crate::core::wallet::{Decision, HandleId, PolicyApproval, TxHandle, TxStatus};
use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_eth::TransactionReceipt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Confirmation depth before a mined tx is treated as final. OZ uses 12 for mainnet;
/// L2s want fewer. Per-chain, tunable via [`AccountExecutor::with_confirmations`].
const DEFAULT_REQUIRED_CONFIRMATIONS: u64 = 12;

/// Resubmit (bump) a tx that has been pending at least this long (seconds) — OZ's
/// time-based resubmit. Tunable via [`AccountExecutor::with_bump_timeout`].
const DEFAULT_BUMP_TIMEOUT_SECS: u64 = 30;

/// The single consistent chain snapshot a confirm cycle works from — read once up
/// front so every per-handle decision uses the same view and finality rule.
struct Cycle {
    view: ChainView,
    finality: FinalityConfig,
    mined_nonce: u64,
}

/// Per-account tracking executor (thirdweb engine-core pattern): the nonce is
/// per-account, so one executor serializes an account's in-flight txs. The host
/// drives [`tick`](Self::tick) on a `Clock` cadence; each tick runs
/// Recover → Confirm → Escalate.
pub struct AccountExecutor {
    rpc: Arc<dyn Rpc>,
    nonce_manager: Arc<dyn NonceManager>,
    submission: Arc<dyn SubmissionStrategy>,
    state_store: Arc<dyn StateStore>,
    gas_oracle: Arc<dyn GasOracle>,
    policy: Arc<dyn PolicyEngine>,
    signer: Arc<dyn Signer>,
    clock: Arc<dyn Clock>,
    account: Address,
    required_confirmations: u64,
    bump_timeout: u64,
    /// Lossy cache of the one thing a handle can't persist — the bump approval
    /// capability. A cache miss just means "re-evaluate policy on the next bump", so
    /// every persisted handle is bump-eligible with or without an entry here.
    approvals: Mutex<HashMap<HandleId, PolicyApproval>>,
    /// Highest `latest` block seen; a lower reading next cycle is a lagging node and
    /// the cycle is skipped (a stale head must not drive transitions).
    last_latest: AtomicU64,
}

impl AccountExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc: Arc<dyn Rpc>,
        nonce_manager: Arc<dyn NonceManager>,
        submission: Arc<dyn SubmissionStrategy>,
        state_store: Arc<dyn StateStore>,
        gas_oracle: Arc<dyn GasOracle>,
        policy: Arc<dyn PolicyEngine>,
        signer: Arc<dyn Signer>,
        clock: Arc<dyn Clock>,
        account: Address,
    ) -> Self {
        // The executor signs bumps for `account`; a signer for a different key would put
        // them on the wrong sender. This is a wiring invariant (the facade enforces it).
        debug_assert_eq!(
            signer.address(),
            account,
            "AccountExecutor signer must control the account"
        );
        Self {
            rpc,
            nonce_manager,
            submission,
            state_store,
            gas_oracle,
            policy,
            signer,
            clock,
            account,
            required_confirmations: DEFAULT_REQUIRED_CONFIRMATIONS,
            bump_timeout: DEFAULT_BUMP_TIMEOUT_SECS,
            approvals: Mutex::new(HashMap::new()),
            last_latest: AtomicU64::new(0),
        }
    }

    /// Override the confirmation depth (per-chain reorg table).
    pub fn with_confirmations(mut self, depth: u64) -> Self {
        self.required_confirmations = depth;
        self
    }

    /// Override the pending-before-bump timeout (seconds).
    pub fn with_bump_timeout(mut self, secs: u64) -> Self {
        self.bump_timeout = secs;
        self
    }

    /// Seed the approval cache for a freshly-sent tx so its first bump reuses the
    /// approval instead of re-evaluating policy. Optional: a bump with no cached
    /// approval simply re-evaluates from the handle's persisted intent.
    pub fn track(&self, handle: HandleId, approval: PolicyApproval) {
        self.approvals.lock().insert(handle, approval);
    }

    /// One executor cycle: recover in-flight txs, confirm progress, bump the stuck.
    /// The host calls this per `Clock` tick.
    pub async fn tick(&self) -> Result<(), ExecutorError> {
        self.recover().await?;
        self.confirm().await?;
        self.escalate().await
    }

    /// Rebroadcast persisted txs that should be in the mempool (Pending/Sent).
    /// Idempotent — an already-known tx is fine — so a per-handle submit failure is
    /// skipped; only a store read failure aborts the cycle. Mined/Replacing handles
    /// are Confirm's job (rebroadcasting them only earns a nonce-too-low).
    pub async fn recover(&self) -> Result<(), ExecutorError> {
        for handle in self.state_store.pending_handles(self.account).await? {
            if matches!(handle.status, TxStatus::Pending | TxStatus::Sent) {
                let _ = self.submission.submit(handle.signed.clone()).await;
            }
        }
        Ok(())
    }

    /// Advance in-flight handles against one consistent chain view: reconcile the nonce
    /// forward, then move each handle by the pure [`transition`]. Every unreliable read
    /// collapses to [`ChainEvent::Unknown`] (per handle) or a skipped cycle (bad view),
    /// so a wrong read can neither advance nor rewind the lifecycle.
    pub async fn confirm(&self) -> Result<(), ExecutorError> {
        let Some(cycle) = self.read_cycle().await? else {
            return Ok(()); // stale/inconsistent head — skip the whole cycle
        };
        // Reconcile the allocator forward — a foreign/out-of-band tx can advance the
        // chain nonce without our allocation.
        self.nonce_manager
            .reset(self.account, cycle.mined_nonce)
            .await?;
        for mut handle in self.state_store.pending_handles(self.account).await? {
            let event = self.event_for(&handle, cycle.mined_nonce).await;
            let Some(next) = transition(&handle.status, &event, &cycle.view, &cycle.finality)
            else {
                continue;
            };
            handle.status = next;
            // A terminal handle no longer needs its cached approval.
            if self.state_store.put_handle(&handle).await.is_ok() && handle.status.is_terminal() {
                self.approvals.lock().remove(&handle.id);
            }
        }
        Ok(())
    }

    /// Read one consistent [`Cycle`] snapshot and resolve its finality rule: prefer the
    /// `finalized` tag, fall back to a depth count when the chain lacks it. Returns
    /// `None` — skip the cycle — when the head regressed since last cycle or `finalized`
    /// is above `latest`, both signs of a stale/lagging node.
    async fn read_cycle(&self) -> Result<Option<Cycle>, ExecutorError> {
        let latest = self.rpc.block_number().await?;
        let mined_nonce = self.rpc.tx_count(self.account).await?;
        if latest < self.last_latest.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let (mode, finalized) = match self.rpc.finalized_block().await? {
            Some(finalized) => (Finality::Finalized, finalized),
            None => (Finality::Depth, 0),
        };
        if finalized > latest {
            return Ok(None);
        }
        self.last_latest.store(latest, Ordering::Relaxed);
        Ok(Some(Cycle {
            view: ChainView { latest, finalized },
            finality: FinalityConfig {
                mode,
                required: self.required_confirmations,
            },
            mined_nonce,
        }))
    }

    /// Distill one handle's chain reads into a single trustworthy [`ChainEvent`]. Our
    /// own tx is the strongest evidence, so a hash-anchored receipt wins over the nonce
    /// count — a lagging `tx_count` can neither hide nor un-mine a canonical tx. With no
    /// receipt, the nonce decides: consumed by a foreign tx (`Replaced`) or still ours
    /// (`Pending`). Any read error or non-canonical receipt is `Unknown`.
    async fn event_for(&self, handle: &TxHandle, mined: u64) -> ChainEvent {
        // Newest-first: after an RBF bump the latest replacement is the one that mines.
        for hash in handle.broadcasts.iter().rev() {
            match self.rpc.receipt(*hash).await {
                Ok(None) => continue, // this broadcast isn't mined; try an older one
                Ok(Some(receipt)) => return self.anchor(receipt).await,
                Err(_) => return ChainEvent::Unknown,
            }
        }
        match handle.nonce < mined {
            true => ChainEvent::Replaced, // a foreign tx consumed our nonce
            false => ChainEvent::Pending, // still in the mempool
        }
    }

    /// Trust a receipt only if its block is still canonical — `block_hash(n)` must equal
    /// the receipt's hash (geth serves receipts from stale forks after a reorg). A
    /// non-canonical block, a receipt with no block anchor, or a read error is `Unknown`.
    async fn anchor(&self, receipt: TransactionReceipt) -> ChainEvent {
        let (Some(block), Some(block_hash)) = (receipt.block_number, receipt.block_hash) else {
            return ChainEvent::Unknown;
        };
        match self.rpc.block_hash(block).await {
            Ok(Some(canonical)) if canonical == block_hash => ChainEvent::Mined {
                block,
                block_hash,
                outcome: match receipt.status() {
                    true => Outcome::Executed,
                    false => Outcome::Reverted,
                },
            },
            _ => ChainEvent::Unknown,
        }
    }

    /// Bump every still-pending tx that has outstayed the timeout: raise its fees at
    /// the **same nonce** (RBF). Everything a bump needs comes from the persisted
    /// handle (only the approval is a cache). Best-effort per handle — one failure
    /// doesn't abort the cycle.
    pub async fn escalate(&self) -> Result<(), ExecutorError> {
        let now = self.clock.now_unix();
        for mut handle in self.state_store.pending_handles(self.account).await? {
            let stuck = matches!(handle.status, TxStatus::Sent)
                && now.saturating_sub(handle.last_broadcast_at) >= self.bump_timeout;
            if stuck {
                let _ = self.bump(&mut handle, now).await;
            }
        }
        Ok(())
    }

    async fn bump(&self, handle: &mut TxHandle, now: u64) -> Result<(), ExecutorError> {
        // Re-check right before bumping: if the nonce mined since the handle was
        // selected, don't broadcast a doomed replacement — Confirm will settle it.
        if handle.nonce < self.rpc.tx_count(self.account).await? {
            return Ok(());
        }
        // The current fees + gas limit live in the persisted signed tx.
        let Some((fees, gas_limit)) = decode_fees(&handle.signed) else {
            return Ok(()); // undecodable (never our own tx) -> leave it
        };
        let bumped = match self.gas_oracle.bump(fees).await {
            Ok(fees) => fees,
            // At the ceiling we stop and leave the tx as-is (an operator signal, not a retry).
            Err(GasOracleError::CeilingExceeded { .. }) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // The originally-approved envelope is a hard per-intent spend ceiling. A bump
        // beyond it stops (operator signal) rather than silently escalating the approved
        // spend — raising it requires a fresh authorization (Phase 2/3 refill).
        if !handle
            .envelope
            .admits(bumped.max_fee_per_gas, bumped.max_priority_fee_per_gas)
        {
            return Ok(());
        }
        let Some(approval) = self.bump_approval(handle, bumped, now).await? else {
            return Ok(()); // policy revoked or tightened below the bump -> leave the tx
        };

        let tx = signing::build_tx(&handle.intent, handle.nonce, gas_limit, bumped);
        let (rlp, tx_hash) =
            signing::sign_encode(&*self.signer, tx, handle.intent_hash, &approval, now).await?;
        match self.submission.submit(rlp.clone()).await {
            Ok(_) => {}
            // Already in the mempool (a prior round's replacement): record it, not an error.
            Err(e) if e.is_already_accepted() => {}
            Err(e) => return Err(e.into()),
        }

        handle.signed = rlp;
        handle.broadcasts.push(tx_hash);
        handle.last_broadcast_at = now;
        self.state_store.put_handle(handle).await?;
        self.approvals.lock().insert(handle.id, approval);
        Ok(())
    }

    /// The approval to sign a bump with: reuse the cached one while it is valid and
    /// covers the bumped fees, else re-evaluate policy from the persisted intent (which
    /// mirrors the signer gate, so a lapsed TTL can't wedge RBF). `None` means policy
    /// denied or returned an envelope that no longer covers the bump — leave the tx.
    async fn bump_approval(
        &self,
        handle: &TxHandle,
        bumped: Eip1559Estimation,
        now: u64,
    ) -> Result<Option<PolicyApproval>, ExecutorError> {
        let admits = |a: &PolicyApproval| {
            a.gas_envelope()
                .admits(bumped.max_fee_per_gas, bumped.max_priority_fee_per_gas)
        };
        let cached = self.approvals.lock().get(&handle.id).cloned();
        if let Some(approval) = cached
            && approval.valid_until() >= now
            && admits(&approval)
        {
            return Ok(Some(approval));
        }
        match self.policy.evaluate(&handle.intent).await? {
            Decision::Allow(approval) => Ok(admits(&approval).then_some(approval)),
            Decision::Deny(_) => Ok(None),
        }
    }
}

/// Recover the fee fields + gas limit from a persisted EIP-1559 signed tx (Phase 1
/// only builds 1559); anything else can't be bumped by this path.
fn decode_fees(signed: &Bytes) -> Option<(Eip1559Estimation, u64)> {
    match TxEnvelope::decode_2718(&mut signed.as_ref()).ok()? {
        TxEnvelope::Eip1559(signed_tx) => {
            let tx = signed_tx.tx();
            let fees = Eip1559Estimation {
                max_fee_per_gas: tx.max_fee_per_gas,
                max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
            };
            Some((fees, tx.gas_limit))
        }
        _ => None,
    }
}

/// Operational failures the executor surfaces from the ports it drives. Its own type
/// (not the pipeline's) — tracking and one-shot send are separate concerns.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutorError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Gas(#[from] GasOracleError),
    #[error(transparent)]
    Policy(#[from] PolicyEngineError),
    #[error(transparent)]
    Nonce(#[from] NonceManagerError),
    #[error(transparent)]
    Signer(#[from] SignerError),
    #[error(transparent)]
    Store(#[from] StateStoreError),
    #[error(transparent)]
    Submission(#[from] SubmissionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::wallet::GasEnvelope;
    use crate::testutils::{
        Harness, MockClock, MockGas, MockPolicy, MockRpc, MockStore, MockSubmit, Submit,
        estimation, handle, receipt,
    };
    use alloy_primitives::B256;

    // --- Recover / confirm ---

    #[tokio::test]
    async fn recover_rebroadcasts_persisted_inflight_after_restart() {
        let store = Arc::new(MockStore::default());
        let h = handle(4, TxStatus::Sent);
        store.put_handle(&h).await.unwrap();
        let submit = Arc::new(MockSubmit::default());
        let exec = Harness::default()
            .submit(submit.clone())
            .store(store.clone())
            .executor();
        exec.recover().await.unwrap();
        assert_eq!(*submit.seen.lock(), vec![h.signed]);
    }

    /// Run one confirm cycle against a fixed chain view. `canonical` is the hash
    /// `block_hash(_)` returns, so a receipt anchors only when it matches. Depth mode
    /// (no `finalized` tag), `confirmations` required.
    async fn run_confirm(
        store: &Arc<MockStore>,
        tx_count: u64,
        head: u64,
        receipt: Option<TransactionReceipt>,
        canonical: Option<B256>,
        confirmations: u64,
    ) {
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count,
                block_number: head,
                receipt,
                canonical,
                ..Default::default()
            }))
            .store(store.clone())
            .confirmations(confirmations)
            .executor()
            .confirm()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn confirm_advances_on_anchored_receipt_at_required_depth() {
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        let h = B256::repeat_byte(1);
        // receipt at block 8 anchors to the canonical hash; head 10 -> depth 3 >= 2.
        run_confirm(&store, 5, 10, Some(receipt(true, 8, h)), Some(h), 2).await;
        assert_eq!(store.all()[0].status, TxStatus::Confirmed { block: 8 });
    }

    #[tokio::test]
    async fn reverted_receipt_fails_only_at_depth() {
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        let h = B256::repeat_byte(1);
        run_confirm(&store, 5, 10, Some(receipt(false, 8, h)), Some(h), 2).await;
        assert!(matches!(store.all()[0].status, TxStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn stale_receipt_from_a_reorged_block_holds_the_state() {
        // The crux at the shell: our receipt claims block 8/hash h2, but the canonical
        // hash at 8 is h1 -> the read is stale -> Unknown -> no transition.
        let store = Arc::new(MockStore::default());
        let mined = TxStatus::Mined {
            block: 8,
            block_hash: B256::repeat_byte(1),
        };
        store.put_handle(&handle(4, mined.clone())).await.unwrap();
        run_confirm(
            &store,
            5,
            10,
            Some(receipt(true, 8, B256::repeat_byte(2))),
            Some(B256::repeat_byte(1)),
            2,
        )
        .await;
        assert_eq!(store.all()[0].status, mined); // unchanged
    }

    #[tokio::test]
    async fn freed_nonce_un_mines_a_tentative_handle() {
        // A reorg dropped our mined tx and freed the nonce (tx_count back to our nonce)
        // with no receipt remaining -> Pending -> re-track from Sent.
        let store = Arc::new(MockStore::default());
        store
            .put_handle(&handle(
                4,
                TxStatus::Mined {
                    block: 8,
                    block_hash: B256::repeat_byte(1),
                },
            ))
            .await
            .unwrap();
        run_confirm(&store, 4, 10, None, None, 12).await;
        assert_eq!(store.all()[0].status, TxStatus::Sent);
    }

    #[tokio::test]
    async fn replacement_is_tentative_until_depth_then_final() {
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        // nonce consumed (mined 5 > 4) but none of our broadcasts mined -> a foreign tx.
        run_confirm(&store, 5, 10, None, None, 3).await;
        assert_eq!(
            store.all()[0].status,
            TxStatus::Replacing { since_block: 10 } // tentative, not yet final
        );
        // Head advances past the depth window -> final.
        run_confirm(&store, 5, 13, None, None, 3).await;
        assert_eq!(store.all()[0].status, TxStatus::Replaced);
    }

    #[tokio::test]
    async fn replacement_reorg_frees_the_nonce_and_recovers_to_sent() {
        let store = Arc::new(MockStore::default());
        store
            .put_handle(&handle(4, TxStatus::Replacing { since_block: 5 }))
            .await
            .unwrap();
        // A reorg dropped the replacing tx: the mined nonce fell back to 4, so our
        // nonce is free again and our tx must be re-tracked.
        run_confirm(&store, 4, 10, None, None, 12).await;
        assert_eq!(store.all()[0].status, TxStatus::Sent);
    }

    #[tokio::test]
    async fn finalized_tag_gates_terminality() {
        // With a finalized tag, a receipt is terminal only once its block <= finalized,
        // regardless of how far ahead `latest` is.
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        let h = B256::repeat_byte(1);
        let confirm_with_finalized = async |finalized: u64| {
            Harness::default()
                .rpc(Arc::new(MockRpc {
                    tx_count: 5,
                    block_number: 100,
                    finalized: Some(finalized),
                    receipt: Some(receipt(true, 8, h)),
                    canonical: Some(h),
                    ..Default::default()
                }))
                .store(store.clone())
                .executor()
                .confirm()
                .await
                .unwrap();
        };
        // finalized 7 < block 8 -> tentative Mined despite the far-ahead head.
        confirm_with_finalized(7).await;
        assert_eq!(
            store.all()[0].status,
            TxStatus::Mined {
                block: 8,
                block_hash: h
            }
        );
        // finalized advances to 8 -> now irreversible.
        confirm_with_finalized(8).await;
        assert_eq!(store.all()[0].status, TxStatus::Confirmed { block: 8 });
    }

    #[tokio::test]
    async fn inconsistent_view_skips_the_cycle() {
        // finalized above latest is a stale/inconsistent read -> skip, no transition,
        // even though the receipt would otherwise confirm.
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        let h = B256::repeat_byte(1);
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 5,
                block_number: 100,
                finalized: Some(200), // > latest -> inconsistent
                receipt: Some(receipt(true, 8, h)),
                canonical: Some(h),
                ..Default::default()
            }))
            .store(store.clone())
            .executor()
            .confirm()
            .await
            .unwrap();
        assert_eq!(store.all()[0].status, TxStatus::Sent); // unchanged
    }

    // --- Send / bump ---

    /// Executor wired for a single stuck (Sent, nonce 4) tx at clock 1000, bump
    /// timeout disabled; `bump` is the oracle's next fees (`None` = at ceiling).
    fn send_setup(
        bump: Option<Eip1559Estimation>,
    ) -> (AccountExecutor, Arc<MockStore>, Arc<MockPolicy>) {
        let store = Arc::new(MockStore::default());
        let policy = Arc::new(MockPolicy::default());
        let exec = Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 4,
                ..Default::default()
            }))
            .gas(Arc::new(MockGas {
                bump,
                ..Default::default()
            }))
            .policy(policy.clone())
            .clock(Arc::new(MockClock(1000)))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        (exec, store, policy)
    }

    async fn seed_and_track(exec: &AccountExecutor, store: &MockStore, envelope: GasEnvelope) {
        let mut h = handle(4, TxStatus::Sent);
        h.envelope = envelope;
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, envelope, u64::MAX),
        );
    }

    #[tokio::test]
    async fn bump_within_envelope_reuses_approval() {
        let (exec, store, policy) = send_setup(Some(estimation(200, 1)));
        seed_and_track(&exec, &store, GasEnvelope::DEFAULT).await; // wide -> 200 admitted
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 2); // original + bump
        assert_eq!(*policy.calls.lock(), 0); // approval reused, no re-policy
    }

    #[tokio::test]
    async fn bump_beyond_approved_envelope_stops() {
        // Bumped fees exceed the original per-intent ceiling -> hard-stop, no silent
        // widening: no new broadcast and no re-policy.
        let (exec, store, policy) = send_setup(Some(estimation(200, 1)));
        let tight = GasEnvelope {
            max_fee_cap: 150,
            max_priority_cap: 150,
        };
        seed_and_track(&exec, &store, tight).await; // 200 > 150 -> stop
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 1);
        assert_eq!(*policy.calls.lock(), 0);
    }

    #[tokio::test]
    async fn bump_refreshes_expired_approval_within_envelope() {
        // Approval expired (valid_until 0 < clock 1000) but the bump stays within the
        // envelope -> refresh via policy (not stuck), then bump.
        let (exec, store, policy) = send_setup(Some(estimation(200, 1)));
        let h = handle(4, TxStatus::Sent);
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, GasEnvelope::DEFAULT, 0),
        );
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 2); // bumped
        assert_eq!(*policy.calls.lock(), 1); // refreshed (was expired), not reused
    }

    #[tokio::test]
    async fn bump_stops_at_gas_ceiling() {
        let (exec, store, policy) = send_setup(None); // gas oracle at ceiling
        seed_and_track(&exec, &store, GasEnvelope::DEFAULT).await;
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 1); // no new broadcast
        assert_eq!(*policy.calls.lock(), 0);
    }

    #[tokio::test]
    async fn bump_records_broadcast_when_already_known() {
        // "already known" == the replacement is already in the mempool -> record it as a
        // broadcast (so Confirm can match its receipt), not an error that drops it.
        let store = Arc::new(MockStore::default());
        let exec = Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 4,
                ..Default::default()
            }))
            .gas(Arc::new(MockGas {
                bump: Some(estimation(200, 1)),
                ..Default::default()
            }))
            .submit(Arc::new(MockSubmit {
                outcome: Submit::AlreadyKnown,
                ..Default::default()
            }))
            .clock(Arc::new(MockClock(1000)))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        let h = handle(4, TxStatus::Sent);
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, GasEnvelope::DEFAULT, u64::MAX),
        );
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 2);
    }

    #[tokio::test]
    async fn bump_aborts_if_nonce_already_mined() {
        // tx_count advanced past our nonce between selection and bump -> no doomed
        // replacement broadcast; Confirm will settle it.
        let store = Arc::new(MockStore::default());
        let exec = Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 5, // our nonce 4 is already mined
                ..Default::default()
            }))
            .gas(Arc::new(MockGas {
                bump: Some(estimation(200, 1)),
                ..Default::default()
            }))
            .clock(Arc::new(MockClock(1000)))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        let h = handle(4, TxStatus::Sent);
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, GasEnvelope::DEFAULT, u64::MAX),
        );
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 1); // aborted, no bump
    }
}
