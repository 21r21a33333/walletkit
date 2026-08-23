//! `AccountExecutor` — the per-account tracking loop that turns broadcast txs into
//! terminal outcomes. Separate from the one-shot send pipeline
//! ([`TransactionManager`](super::TransactionManager)) because it is a distinct
//! concern: the send path builds and submits once, the executor tracks forever.

use crate::core::deps::{
    Clock, GasOracle, GasOracleError, NonceManager, PolicyEngine, Rpc, Signer, StateStore,
    SubmissionStrategy,
};
use crate::core::wallet::{
    Decision, GasEnvelope, HandleId, PolicyApproval, TransactionManagerError, TxHandle, TxIntent,
    TxStatus,
};
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_eth::TransactionReceipt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Confirmation depth before a mined tx is treated as final. OZ uses 12 for mainnet;
/// L2s want fewer. Per-chain, tunable via [`AccountExecutor::with_confirmations`].
const DEFAULT_REQUIRED_CONFIRMATIONS: u64 = 12;

/// Resubmit (bump) a tx that has been pending at least this long (seconds) — OZ's
/// time-based resubmit. Tunable via [`AccountExecutor::with_bump_timeout`].
const DEFAULT_BUMP_TIMEOUT_SECS: u64 = 30;

/// In-memory per-tx state the executor needs to bump — none of it is persisted. The
/// `approval` is a bounded-reuse capability (never on disk); `intent` lets us refresh
/// an expired approval; `envelope` is the immutable per-intent spend ceiling a bump
/// must never exceed; `fees` is what we bump from.
#[derive(Clone)]
struct TrackedTx {
    intent: TxIntent,
    approval: PolicyApproval,
    envelope: GasEnvelope,
    gas_limit: u64,
    fees: Eip1559Estimation,
    last_broadcast_at: u64,
}

/// Per-account tracking executor (thirdweb engine-core pattern): the nonce is
/// per-account, so one executor serializes an account's in-flight txs. The host
/// drives [`tick`](Self::tick) on a `Clock` cadence; each tick runs
/// Recover → Confirm → Send.
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
    tracking: Mutex<HashMap<HandleId, TrackedTx>>,
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
            tracking: Mutex::new(HashMap::new()),
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

    /// Register a freshly-sent tx for tracking and bumping — the send pipeline hands
    /// off the in-memory state (approval + intent + fees) the executor can't persist.
    pub fn track(
        &self,
        handle: HandleId,
        intent: TxIntent,
        approval: PolicyApproval,
        gas_limit: u64,
        fees: Eip1559Estimation,
    ) {
        let last_broadcast_at = self.clock.now_unix();
        let envelope = approval.gas_envelope(); // the immutable per-intent spend ceiling
        self.tracking.lock().unwrap().insert(
            handle,
            TrackedTx {
                intent,
                approval,
                envelope,
                gas_limit,
                fees,
                last_broadcast_at,
            },
        );
    }

    /// One executor cycle: recover in-flight txs, confirm progress, bump the stuck.
    /// The host calls this per `Clock` tick.
    pub async fn tick(&self) -> Result<(), TransactionManagerError> {
        self.recover().await?;
        self.confirm().await?;
        self.send().await
    }

    /// Rebroadcast persisted txs that should be in the mempool (Pending/Sent).
    /// Idempotent — an already-known tx is fine — so a per-handle submit failure is
    /// skipped; only a store read failure aborts the cycle. Mined/Replacing handles
    /// are Confirm's job (rebroadcasting them only earns a nonce-too-low).
    pub async fn recover(&self) -> Result<(), TransactionManagerError> {
        for handle in self.state_store.pending_handles(self.account).await? {
            if matches!(handle.status, TxStatus::Pending | TxStatus::Sent) {
                let _ = self.submission.submit(handle.signed.clone()).await;
            }
        }
        Ok(())
    }

    /// Classify in-flight handles by nonce progression: once the account's mined
    /// nonce passes a handle's nonce, the receipt says mined/confirmed/failed, and a
    /// nonce consumed by a hash that isn't ours means it was replaced.
    pub async fn confirm(&self) -> Result<(), TransactionManagerError> {
        let mined = self.rpc.tx_count(self.account).await?;
        let head = self.rpc.block_number().await?;
        // Reconcile the allocator forward — a foreign/out-of-band tx can advance the
        // chain nonce without our allocation.
        self.nonce_manager.reset(self.account, mined).await?;
        for mut handle in self.state_store.pending_handles(self.account).await? {
            // Per-handle failures are non-fatal: a transient receipt read on one handle
            // must not block confirming/bumping the rest (matches recover()/send()).
            let Ok(Some(status)) = self.classify(&handle, mined, head).await else {
                continue;
            };
            handle.status = status;
            // A terminal handle no longer needs its in-memory bump state.
            if self.state_store.put_handle(&handle).await.is_ok() && handle.status.is_terminal() {
                self.tracking.lock().unwrap().remove(&handle.id);
            }
        }
        Ok(())
    }

    /// The next status for one handle, or `None` to leave it unchanged. Only outcomes
    /// at `required_confirmations` depth are terminal; shallower ones stay trackable so
    /// a reorg can recover them.
    async fn classify(
        &self,
        handle: &TxHandle,
        mined: u64,
        head: u64,
    ) -> Result<Option<TxStatus>, TransactionManagerError> {
        if handle.nonce >= mined {
            // Nonce not consumed on-chain. A reorg that un-mined/un-replaced it frees the
            // nonce, so a tentative Mined/Replacing handle goes back to Sent for rebroadcast.
            return Ok(matches!(
                handle.status,
                TxStatus::Mined { .. } | TxStatus::Replacing { .. }
            )
            .then_some(TxStatus::Sent));
        }
        match self.our_receipt(handle).await? {
            Some(r) => {
                let block = r.block_number.unwrap_or(head);
                let block_hash = r.block_hash.unwrap_or_default();
                // Re-mined in a different block than last seen -> reorg, re-track from Sent.
                if let TxStatus::Mined {
                    block_hash: prev, ..
                } = handle.status
                    && prev != block_hash
                {
                    return Ok(Some(TxStatus::Sent));
                }
                if head.saturating_sub(block) + 1 < self.required_confirmations {
                    // In a block but not final; skip a redundant rewrite if unchanged.
                    return Ok(match handle.status {
                        TxStatus::Mined {
                            block_hash: prev, ..
                        } if prev == block_hash => None,
                        _ => Some(TxStatus::Mined { block, block_hash }),
                    });
                }
                Ok(Some(if r.status() {
                    TxStatus::Confirmed { block }
                } else {
                    TxStatus::Failed {
                        reason: "reverted on-chain".into(),
                    }
                }))
            }
            // A foreign hash consumed our nonce; depth-gate before declaring it final so a
            // reorg that frees the nonce (handled above) can still recover our tx.
            None => Ok(match handle.status {
                TxStatus::Replacing { since_block } => (head.saturating_sub(since_block)
                    >= self.required_confirmations)
                    .then_some(TxStatus::Replaced),
                _ => Some(TxStatus::Replacing { since_block: head }),
            }),
        }
    }

    /// The receipt of whichever of our broadcasts mined (`None` if none did). Scans
    /// newest-first: after an RBF bump the latest replacement is the one that can mine.
    async fn our_receipt(
        &self,
        handle: &TxHandle,
    ) -> Result<Option<TransactionReceipt>, TransactionManagerError> {
        for hash in handle.broadcasts.iter().rev() {
            if let Some(receipt) = self.rpc.receipt(*hash).await? {
                return Ok(Some(receipt));
            }
        }
        Ok(None)
    }

    /// Bump every still-pending tx that has outstayed the timeout: raise its fees at
    /// the **same nonce** (that is RBF), reusing the approval if the new fees stay in
    /// its envelope, else re-evaluating policy. Stops (leaves the tx) at the gas
    /// ceiling. Best-effort per handle — one failure doesn't abort the cycle.
    pub async fn send(&self) -> Result<(), TransactionManagerError> {
        let mined = self.rpc.tx_count(self.account).await?;
        let now = self.clock.now_unix();
        for mut handle in self.state_store.pending_handles(self.account).await? {
            // Only txs still in the mempool (Sent, nonce not yet mined) are bumped;
            // the mined region is Confirm's job.
            if handle.nonce < mined || !matches!(handle.status, TxStatus::Sent) {
                continue;
            }
            let mut tracked = match self.tracking.lock().unwrap().get(&handle.id).cloned() {
                Some(t) if now.saturating_sub(t.last_broadcast_at) >= self.bump_timeout => t,
                _ => continue,
            };
            if self.bump(&mut handle, &mut tracked, now).await.is_ok() {
                self.tracking.lock().unwrap().insert(handle.id, tracked);
            }
        }
        Ok(())
    }

    async fn bump(
        &self,
        handle: &mut TxHandle,
        tracked: &mut TrackedTx,
        now: u64,
    ) -> Result<(), TransactionManagerError> {
        let bumped = match self.gas_oracle.bump(tracked.fees).await {
            Ok(fees) => fees,
            // At the ceiling we stop and leave the tx as-is (an operator signal, not a retry).
            Err(GasOracleError::CeilingExceeded { .. }) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // The originally-approved envelope is a hard per-intent spend ceiling. A bump
        // beyond it stops (operator signal) rather than silently escalating the approved
        // spend — raising it requires a fresh authorization (Phase 2/3 refill).
        if !tracked
            .envelope
            .admits(bumped.max_fee_per_gas, bumped.max_priority_fee_per_gas)
        {
            return Ok(());
        }
        // Reuse the approval only while it is both valid (unexpired) and covers the bumped
        // fees; otherwise refresh it via policy (its envelope can't exceed the ceiling
        // checked above). This mirrors the signer gate so a lapsed TTL can't wedge RBF.
        let approval = if tracked.approval.valid_until() >= now
            && tracked
                .approval
                .gas_envelope()
                .admits(bumped.max_fee_per_gas, bumped.max_priority_fee_per_gas)
        {
            tracked.approval.clone()
        } else {
            match self.policy.evaluate(&tracked.intent).await? {
                Decision::Allow(approval) => approval,
                Decision::Deny(_) => return Ok(()), // policy revoked -> leave the tx
            }
        };

        let tx = TxEip1559 {
            chain_id: tracked.intent.chain_id,
            nonce: handle.nonce,
            gas_limit: tracked.gas_limit,
            max_fee_per_gas: bumped.max_fee_per_gas,
            max_priority_fee_per_gas: bumped.max_priority_fee_per_gas,
            to: tracked.intent.to,
            value: tracked.intent.value,
            input: tracked.intent.input.clone(),
            access_list: Default::default(),
        };
        let signature = self
            .signer
            .sign_transaction(&tx, handle.intent_hash, &approval, now)
            .await?;
        let signed = tx.into_signed(signature);
        let rlp = Bytes::from(signed.encoded_2718());
        self.submission.submit(rlp.clone()).await?;

        handle.signed = rlp;
        handle.broadcasts.push(*signed.hash());
        self.state_store.put_handle(handle).await?;
        tracked.fees = bumped;
        tracked.approval = approval;
        tracked.last_broadcast_at = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::{
        GasOracleError, NonceManagerError, PolicyEngineError, RpcError, SignerError,
        StateStoreError, SubmissionError, Versioned,
    };
    use crate::core::wallet::{GasEnvelope, IntentHash, NonceScope, NonceState};
    use alloy_primitives::{B256, Signature, TxHash, TxKind, U256};
    use alloy_rpc_types_eth::TransactionRequest;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockGas {
        bumped: Option<Eip1559Estimation>, // None => at ceiling
    }
    #[async_trait]
    impl GasOracle for MockGas {
        async fn bump(&self, _: Eip1559Estimation) -> Result<Eip1559Estimation, GasOracleError> {
            self.bumped.ok_or(GasOracleError::CeilingExceeded {
                ceiling: 0,
                needed: 1,
            })
        }
        async fn estimate(&self) -> Result<Eip1559Estimation, GasOracleError> {
            unreachable!()
        }
    }

    struct CountingPolicy {
        calls: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl PolicyEngine for CountingPolicy {
        async fn evaluate(&self, intent: &TxIntent) -> Result<Decision, PolicyEngineError> {
            *self.calls.lock().unwrap() += 1;
            Ok(Decision::Allow(PolicyApproval::mint(
                intent.hash(),
                GasEnvelope::DEFAULT,
                u64::MAX,
            )))
        }
    }

    struct OkSigner;
    #[async_trait]
    impl Signer for OkSigner {
        fn address(&self) -> Address {
            Address::ZERO
        }
        async fn sign_transaction(
            &self,
            _: &TxEip1559,
            _: IntentHash,
            _: &PolicyApproval,
            _: u64,
        ) -> Result<Signature, SignerError> {
            Ok(Signature::new(U256::from(1), U256::from(1), false))
        }
    }

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn now_unix(&self) -> u64 {
            self.0
        }
    }

    /// A real-enough StateStore: put updates by id, pending returns non-terminal.
    #[derive(Default)]
    struct VecStore {
        handles: Mutex<Vec<TxHandle>>,
    }
    #[async_trait]
    impl StateStore for VecStore {
        async fn put_handle(&self, h: &TxHandle) -> Result<(), StateStoreError> {
            let mut v = self.handles.lock().unwrap();
            match v.iter_mut().find(|x| x.id == h.id) {
                Some(slot) => *slot = h.clone(),
                None => v.push(h.clone()),
            }
            Ok(())
        }
        async fn pending_handles(
            &self,
            account: Address,
        ) -> Result<Vec<TxHandle>, StateStoreError> {
            Ok(self
                .handles
                .lock()
                .unwrap()
                .iter()
                .filter(|h| h.account == account && !h.status.is_terminal())
                .cloned()
                .collect())
        }
        async fn load_nonce_state(
            &self,
            _: NonceScope,
        ) -> Result<Versioned<NonceState>, StateStoreError> {
            unreachable!()
        }
        async fn cas_nonce_state(
            &self,
            _: NonceScope,
            _: u64,
            _: &NonceState,
        ) -> Result<bool, StateStoreError> {
            unreachable!()
        }
    }

    /// Fixed chain view for the Confirm tests.
    struct ConfirmRpc {
        mined: u64,
        head: u64,
        receipt: Option<TransactionReceipt>,
    }
    #[async_trait]
    impl Rpc for ConfirmRpc {
        async fn tx_count(&self, _: Address) -> Result<u64, RpcError> {
            Ok(self.mined)
        }
        async fn block_number(&self) -> Result<u64, RpcError> {
            Ok(self.head)
        }
        async fn receipt(&self, _: TxHash) -> Result<Option<TransactionReceipt>, RpcError> {
            Ok(self.receipt.clone())
        }
        async fn pending_nonce(&self, _: Address) -> Result<u64, RpcError> {
            unreachable!()
        }
        async fn estimate_gas(&self, _: &TransactionRequest) -> Result<u64, RpcError> {
            unreachable!()
        }
        async fn estimate_fees(&self) -> Result<Eip1559Estimation, RpcError> {
            unreachable!()
        }
        async fn base_fee(&self) -> Result<u128, RpcError> {
            unreachable!()
        }
        async fn send_raw(&self, _: Bytes) -> Result<TxHash, RpcError> {
            unreachable!()
        }
    }

    /// Records rebroadcast bytes; accepts everything.
    #[derive(Default)]
    struct RecordSubmit {
        seen: Arc<Mutex<Vec<Bytes>>>,
    }
    #[async_trait]
    impl SubmissionStrategy for RecordSubmit {
        async fn submit(&self, rlp: Bytes) -> Result<TxHash, SubmissionError> {
            self.seen.lock().unwrap().push(rlp);
            Ok(TxHash::ZERO)
        }
    }

    /// Confirm only reconciles the allocator forward; allocate/release never run here.
    struct TrackNonce;
    #[async_trait]
    impl NonceManager for TrackNonce {
        async fn reset(&self, _: Address, _: u64) -> Result<(), NonceManagerError> {
            Ok(())
        }
        async fn allocate(&self, _: Address) -> Result<u64, NonceManagerError> {
            unreachable!()
        }
        async fn release(&self, _: Address, _: u64) -> Result<(), NonceManagerError> {
            unreachable!()
        }
    }

    fn receipt(success: bool, block: u64, block_hash: B256) -> TransactionReceipt {
        use alloy_consensus::{Receipt, ReceiptEnvelope, ReceiptWithBloom};
        TransactionReceipt {
            inner: ReceiptEnvelope::Eip1559(ReceiptWithBloom {
                receipt: Receipt {
                    status: success.into(),
                    cumulative_gas_used: 0,
                    logs: vec![],
                },
                logs_bloom: Default::default(),
            }),
            transaction_hash: TxHash::ZERO,
            transaction_index: None,
            block_hash: Some(block_hash),
            block_number: Some(block),
            gas_used: 0,
            effective_gas_price: 0,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        }
    }

    fn inflight(nonce: u64, status: TxStatus) -> TxHandle {
        TxHandle {
            id: HandleId::new(B256::ZERO, nonce),
            account: Address::ZERO,
            intent_hash: B256::ZERO,
            nonce,
            status,
            signed: Bytes::from_static(&[1, 2, 3]),
            broadcasts: vec![TxHash::ZERO],
        }
    }

    fn executor(rpc: Arc<dyn Rpc>, store: Arc<VecStore>) -> AccountExecutor {
        AccountExecutor::new(
            rpc,
            Arc::new(TrackNonce),
            Arc::new(RecordSubmit::default()),
            store,
            Arc::new(MockGas { bumped: None }),
            Arc::new(CountingPolicy {
                calls: Arc::new(Mutex::new(0)),
            }),
            Arc::new(OkSigner),
            Arc::new(FixedClock(0)),
            Address::ZERO,
        )
    }

    #[tokio::test]
    async fn recover_rebroadcasts_persisted_inflight_after_restart() {
        let store = Arc::new(VecStore::default());
        store
            .put_handle(&inflight(4, TxStatus::Sent))
            .await
            .unwrap();
        let submit = Arc::new(RecordSubmit::default());
        let exec = AccountExecutor::new(
            Arc::new(ConfirmRpc {
                mined: 0,
                head: 0,
                receipt: None,
            }),
            Arc::new(TrackNonce),
            submit.clone(),
            store,
            Arc::new(MockGas { bumped: None }),
            Arc::new(CountingPolicy {
                calls: Arc::new(Mutex::new(0)),
            }),
            Arc::new(OkSigner),
            Arc::new(FixedClock(0)),
            Address::ZERO,
        );
        exec.recover().await.unwrap();
        assert_eq!(
            *submit.seen.lock().unwrap(),
            vec![Bytes::from_static(&[1, 2, 3])]
        );
    }

    #[tokio::test]
    async fn confirm_advances_on_nonce_progression_at_required_depth() {
        let store = Arc::new(VecStore::default());
        store
            .put_handle(&inflight(4, TxStatus::Sent))
            .await
            .unwrap();
        // nonce 4 < mined 5; receipt at block 8, head 10 -> depth 3 >= 2.
        let rpc = ConfirmRpc {
            mined: 5,
            head: 10,
            receipt: Some(receipt(true, 8, B256::repeat_byte(1))),
        };
        executor(Arc::new(rpc), store.clone())
            .with_confirmations(2)
            .confirm()
            .await
            .unwrap();
        assert_eq!(
            store.handles.lock().unwrap()[0].status,
            TxStatus::Confirmed { block: 8 }
        );
    }

    #[tokio::test]
    async fn reverted_receipt_fails_only_at_depth() {
        let store = Arc::new(VecStore::default());
        store
            .put_handle(&inflight(4, TxStatus::Sent))
            .await
            .unwrap();
        let rpc = ConfirmRpc {
            mined: 5,
            head: 10,
            receipt: Some(receipt(false, 8, B256::repeat_byte(1))),
        };
        executor(Arc::new(rpc), store.clone())
            .with_confirmations(2)
            .confirm()
            .await
            .unwrap();
        assert!(matches!(
            store.handles.lock().unwrap()[0].status,
            TxStatus::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn reorg_unmine_returns_handle_to_sent() {
        let store = Arc::new(VecStore::default());
        let h1 = B256::repeat_byte(1);
        store
            .put_handle(&inflight(
                4,
                TxStatus::Mined {
                    block: 8,
                    block_hash: h1,
                },
            ))
            .await
            .unwrap();
        // Same nonce mined, but the receipt now reports a different block hash.
        let rpc = ConfirmRpc {
            mined: 5,
            head: 10,
            receipt: Some(receipt(true, 8, B256::repeat_byte(2))),
        };
        executor(Arc::new(rpc), store.clone())
            .confirm()
            .await
            .unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].status, TxStatus::Sent);
    }

    #[tokio::test]
    async fn replacement_is_tentative_until_depth_then_final() {
        let store = Arc::new(VecStore::default());
        store
            .put_handle(&inflight(4, TxStatus::Sent))
            .await
            .unwrap();
        // nonce consumed (mined 5 > 4) but none of our broadcasts mined.
        executor(
            Arc::new(ConfirmRpc {
                mined: 5,
                head: 10,
                receipt: None,
            }),
            store.clone(),
        )
        .with_confirmations(3)
        .confirm()
        .await
        .unwrap();
        assert_eq!(
            store.handles.lock().unwrap()[0].status,
            TxStatus::Replacing { since_block: 10 } // tentative, not yet final
        );

        // Head advances past the depth window -> final.
        executor(
            Arc::new(ConfirmRpc {
                mined: 5,
                head: 13,
                receipt: None,
            }),
            store.clone(),
        )
        .with_confirmations(3)
        .confirm()
        .await
        .unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].status, TxStatus::Replaced);
    }

    #[tokio::test]
    async fn replacement_reorg_frees_the_nonce_and_recovers_to_sent() {
        let store = Arc::new(VecStore::default());
        store
            .put_handle(&inflight(4, TxStatus::Replacing { since_block: 5 }))
            .await
            .unwrap();
        // A reorg dropped the replacing tx: the account's mined nonce fell back to 4,
        // so our nonce is free again and our tx must be re-tracked.
        executor(
            Arc::new(ConfirmRpc {
                mined: 4,
                head: 10,
                receipt: None,
            }),
            store.clone(),
        )
        .confirm()
        .await
        .unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].status, TxStatus::Sent);
    }

    // --- Send / bump ---

    fn fees(max_fee: u128) -> Eip1559Estimation {
        Eip1559Estimation {
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: 1,
        }
    }

    fn tx_intent() -> TxIntent {
        TxIntent {
            chain_id: 1,
            account: Address::ZERO,
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::new(),
            purpose: None,
        }
    }

    /// Executor wired for a single stuck (Sent, nonce 4) tx, bump timeout disabled.
    fn send_exec(
        bumped: Option<Eip1559Estimation>,
    ) -> (AccountExecutor, Arc<VecStore>, Arc<Mutex<u32>>) {
        let store = Arc::new(VecStore::default());
        let calls = Arc::new(Mutex::new(0));
        let exec = AccountExecutor::new(
            Arc::new(ConfirmRpc {
                mined: 4,
                head: 0,
                receipt: None,
            }),
            Arc::new(TrackNonce),
            Arc::new(RecordSubmit::default()),
            store.clone(),
            Arc::new(MockGas { bumped }),
            Arc::new(CountingPolicy {
                calls: calls.clone(),
            }),
            Arc::new(OkSigner),
            Arc::new(FixedClock(1000)),
            Address::ZERO,
        )
        .with_bump_timeout(0);
        (exec, store, calls)
    }

    async fn seed_and_track(exec: &AccountExecutor, store: &VecStore, envelope: GasEnvelope) {
        let handle = inflight(4, TxStatus::Sent);
        store.put_handle(&handle).await.unwrap();
        let approval = PolicyApproval::mint(B256::ZERO, envelope, u64::MAX);
        exec.track(handle.id, tx_intent(), approval, 21_000, fees(100));
    }

    #[tokio::test]
    async fn bump_within_envelope_reuses_approval() {
        let (exec, store, calls) = send_exec(Some(fees(200)));
        seed_and_track(&exec, &store, GasEnvelope::DEFAULT).await; // wide -> 200 admitted
        exec.send().await.unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].broadcasts.len(), 2); // original + bump
        assert_eq!(*calls.lock().unwrap(), 0); // approval reused, no re-policy
    }

    #[tokio::test]
    async fn bump_beyond_approved_envelope_stops() {
        // Bumped fees exceed the original per-intent ceiling -> hard-stop, no silent
        // widening: no new broadcast and no re-policy.
        let (exec, store, calls) = send_exec(Some(fees(200)));
        let tight = GasEnvelope {
            max_fee_cap: 150,
            max_priority_cap: 150,
        };
        seed_and_track(&exec, &store, tight).await; // 200 > 150 -> stop
        exec.send().await.unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].broadcasts.len(), 1);
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn bump_refreshes_expired_approval_within_envelope() {
        // Approval expired (valid_until 0 < clock 1000) but the bump stays within the
        // envelope -> refresh via policy (not stuck), then bump.
        let (exec, store, calls) = send_exec(Some(fees(200)));
        let handle = inflight(4, TxStatus::Sent);
        store.put_handle(&handle).await.unwrap();
        let expired = PolicyApproval::mint(B256::ZERO, GasEnvelope::DEFAULT, 0);
        exec.track(handle.id, tx_intent(), expired, 21_000, fees(100));
        exec.send().await.unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].broadcasts.len(), 2); // bumped
        assert_eq!(*calls.lock().unwrap(), 1); // refreshed (was expired), not reused
    }

    #[tokio::test]
    async fn bump_stops_at_gas_ceiling() {
        let (exec, store, calls) = send_exec(None); // gas oracle at ceiling
        seed_and_track(&exec, &store, GasEnvelope::DEFAULT).await;
        exec.send().await.unwrap();
        assert_eq!(store.handles.lock().unwrap()[0].broadcasts.len(), 1); // no new broadcast
        assert_eq!(*calls.lock().unwrap(), 0);
    }
}
