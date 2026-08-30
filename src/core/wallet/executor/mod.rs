//! `AccountExecutor` — the per-account tracking loop that turns broadcast txs into
//! terminal outcomes. Separate from the one-shot send pipeline
//! ([`TransactionManager`](super::TransactionManager)) because it is a distinct
//! concern: the send path builds and submits once, the executor tracks forever.
//!
//! The pure state machine it drives lives in [`lifecycle`]; this module is its
//! imperative shell.

mod lifecycle;

pub use lifecycle::{ChainEvent, ChainView, Finality, FinalityConfig, Outcome, transition};

use super::{TransactionManager, signing};
use crate::core::deps::{
    Clock, Escalation, GasOracle, GasOracleError, NonceManager, NonceManagerError, PolicyEngine,
    PolicyEngineError, Relay, RelayStatus, Rpc, RpcError, Signer, SignerError, StateStore,
    StateStoreError, SubmissionError, SubmissionRoute, SubmissionStrategy,
};
use crate::core::wallet::{Decision, HandleId, PolicyApproval, SigningRequest, TxHandle, TxStatus};
use crate::obs::{debug, info, warn};
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::Address;
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
    /// When set, a foreign `Replaced` re-executes the intent through this send pipeline at a
    /// fresh nonce (opt-in intent-refill). `None` disables it.
    refill: Option<Arc<TransactionManager>>,
    /// When set, task-backed handles (a managed Gelato relay: `meta.task = Some`, no tx hash yet)
    /// are polled to inclusion via this port. `None` when no managed relay is configured.
    relay: Option<Arc<dyn Relay>>,
    /// Lossy cache of the one thing a handle can't persist — the bump approval
    /// capability. A cache miss just means "re-evaluate policy on the next bump", so
    /// every persisted handle is bump-eligible with or without an entry here.
    approvals: Mutex<HashMap<HandleId, PolicyApproval>>,
    /// Highest `latest` block seen; a lower reading next cycle is a lagging node and
    /// the cycle is skipped (a stale head must not drive transitions).
    last_latest: AtomicU64,
}

impl AccountExecutor {
    /// Wire an executor for one `account` from the ports it drives.
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
            refill: None,
            relay: None,
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

    /// Enable intent-refill: re-execute an intent displaced by a foreign tx via `manager`'s
    /// send pipeline. Off unless set.
    pub fn with_refill(mut self, manager: Arc<TransactionManager>) -> Self {
        self.refill = Some(manager);
        self
    }

    /// Enable managed-relay polling: task-backed handles (Gelato) are advanced to inclusion via
    /// `relay`. Off unless set — a wallet with no managed relay never polls.
    pub fn with_relay(mut self, relay: Arc<dyn Relay>) -> Self {
        self.relay = Some(relay);
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
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "wallet.tick", level = "debug", skip_all)
    )]
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
            // A managed-relay (Gelato) handle carries no signed bytes — the relay submits it, so
            // there is nothing for us to rebroadcast; the confirm poll advances it instead.
            if matches!(handle.status, TxStatus::Pending | TxStatus::Sent)
                && !handle.signed.is_empty()
            {
                // Re-broadcast on the persisted route — a private tx must not leak to the
                // public mempool after a restart.
                let _ = self
                    .submission
                    .submit(handle.signed.clone(), &handle.submission)
                    .await;
            }
        }
        Ok(())
    }

    /// Advance in-flight handles against one consistent chain view: reconcile the nonce forward,
    /// then advance each handle. Every unreliable read collapses to [`ChainEvent::Unknown`] (per
    /// handle) or a skipped cycle (bad view), so a wrong read can neither advance nor rewind the
    /// lifecycle.
    pub async fn confirm(&self) -> Result<(), ExecutorError> {
        let Some(cycle) = self.read_cycle().await? else {
            return Ok(()); // stale/inconsistent head — skip the whole cycle
        };
        // Reconcile the allocator forward — a foreign/out-of-band tx can advance the chain nonce
        // without our allocation.
        self.nonce_manager
            .reset(self.account, cycle.mined_nonce)
            .await?;
        for mut handle in self.state_store.pending_handles(self.account).await? {
            self.advance_handle(&mut handle, &cycle).await;
        }
        Ok(())
    }

    /// Advance one in-flight handle by whichever tracking applies: a managed-relay handle with no
    /// on-chain hash yet is polled to inclusion; every other handle moves by the pure chain
    /// [`transition`] — and once a polled handle records its hash it follows that same path.
    async fn advance_handle(&self, handle: &mut TxHandle, cycle: &Cycle) {
        if is_awaiting_relay(handle) {
            self.poll_task(handle).await;
        } else {
            self.advance_on_chain(handle, cycle).await;
        }
    }

    /// Move a chain-tracked handle by the pure [`transition`] against `cycle`, then persist it and
    /// run any terminal bookkeeping. A `None` transition (no fresh evidence) leaves it untouched.
    async fn advance_on_chain(&self, handle: &mut TxHandle, cycle: &Cycle) {
        let event = self.event_for(handle, cycle.mined_nonce).await;
        let Some(next) = transition(&handle.status, &event, &cycle.view, &cycle.finality) else {
            return;
        };
        // A cancel whose nonce a foreign tx consumed settles `Dropped`, not `Replaced`.
        let next = match next {
            TxStatus::Replaced if handle.cancelled => TxStatus::Dropped,
            other => other,
        };
        let prev = std::mem::replace(&mut handle.status, next);
        log_transition(handle, &prev);
        // Gate settle-bookkeeping on the persist: a terminal handle that failed to persist stays in
        // `pending_handles` and re-transitions next tick, so acting here would double-spawn refill.
        if self.state_store.put_handle(handle).await.is_ok() && handle.status.is_terminal() {
            self.on_settled(handle).await;
        }
    }

    /// Advance a managed-relay (Gelato) handle by polling its task. Inclusion records the on-chain
    /// hash and drops back to `Sent`, handing off to the chain-confirm path so it anchors at depth
    /// (the relay's `ExecSuccess` verdict is the honest inclusion signal — see [`outcome_of`]); a
    /// relay `Failed` is terminal; a poll error or still-`Pending` task waits for a later tick.
    /// Never a false `Confirmed`: only a genuinely-included, depth-confirmed tx settles.
    async fn poll_task(&self, handle: &mut TxHandle) {
        let (Some(relay), Some(task)) = (
            &self.relay,
            handle.meta.as_ref().and_then(|meta| meta.task.as_ref()),
        ) else {
            return; // no relay wired, or not a task handle — nothing to poll
        };
        match relay.poll(task).await {
            Ok(RelayStatus::Included(hash)) => {
                handle.broadcasts.push(hash);
                handle.status = TxStatus::Sent;
                if self.state_store.put_handle(handle).await.is_ok() {
                    info!(intent_hash = ?handle.intent_hash, "relay task included on-chain");
                }
            }
            Ok(RelayStatus::Failed(reason)) => {
                handle.status = TxStatus::Failed { reason };
                if self.state_store.put_handle(handle).await.is_ok() {
                    self.on_settled(handle).await;
                    info!(intent_hash = ?handle.intent_hash, "relay task failed");
                }
            }
            Ok(RelayStatus::Pending) => {} // still queued — poll again next tick
            Err(_e) => {
                warn!(intent_hash = ?handle.intent_hash, "relay task poll failed; will retry")
            }
        }
    }

    /// Terminal bookkeeping for a just-persisted handle: drop its cached bump approval and, if a
    /// *foreign* tx replaced it (never a cancel — that settled `Dropped`), re-execute the intent
    /// when refill is enabled.
    async fn on_settled(&self, handle: &TxHandle) {
        self.approvals.lock().remove(&handle.id);
        if let Some(manager) = &self.refill
            && handle.status == TxStatus::Replaced
        {
            self.refill_intent(manager, handle).await;
        }
    }

    /// Best-effort re-execution of a displaced intent at a fresh nonce + fresh approval. The
    /// child is a fresh handle, so if it too is displaced it refills again until an attempt
    /// mines. A failure is logged, never aborts the tick.
    async fn refill_intent(&self, manager: &TransactionManager, handle: &TxHandle) {
        // Underscore keeps `_child`/`_err` warning-free when the obs macros are no-ops.
        match manager.send(&handle.intent).await {
            Ok(_child) => debug!(nonce = _child.nonce, "intent refilled after replacement"),
            Err(_err) => warn!(error = %_err, nonce = handle.nonce, "refill failed"),
        }
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
                Ok(Some(receipt)) => return self.anchor(receipt, handle).await,
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
    async fn anchor(&self, receipt: TransactionReceipt, handle: &TxHandle) -> ChainEvent {
        let (Some(block), Some(block_hash)) = (receipt.block_number, receipt.block_hash) else {
            return ChainEvent::Unknown;
        };
        match self.rpc.block_hash(block).await {
            Ok(Some(canonical)) if canonical == block_hash => ChainEvent::Mined {
                block,
                block_hash,
                outcome: outcome_of(&receipt, handle),
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

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            level = "debug",
            skip_all,
            fields(intent_hash = ?handle.intent_hash, nonce = handle.nonce, bump_count = handle.broadcasts.len())
        )
    )]
    async fn bump(&self, handle: &mut TxHandle, now: u64) -> Result<(), ExecutorError> {
        // Re-check right before bumping: if the nonce mined since the handle was
        // selected, don't broadcast a doomed replacement — Confirm will settle it.
        if handle.nonce < self.rpc.tx_count(self.account).await? {
            return Ok(());
        }
        // The current fees + gas limit live in the persisted signed tx.
        let Some(current) = signing::decode_fees(&handle.signed) else {
            return Ok(()); // undecodable (never our own tx) -> leave it
        };
        let bumped = match self.gas_oracle.bump(current.fees).await {
            Ok(fees) => fees,
            // At the ceiling we stop and leave the tx as-is (an operator signal, not a retry).
            Err(GasOracleError::CeilingExceeded { .. }) => {
                warn!(intent_hash = ?handle.intent_hash, "bump halted at gas ceiling");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // The originally-approved envelope is a hard per-intent spend ceiling. A bump
        // beyond it stops (operator signal) rather than silently escalating the approved
        // spend — raising it requires a fresh authorization.
        if !handle
            .envelope
            .admits(bumped.max_fee_per_gas, bumped.max_priority_fee_per_gas)
        {
            warn!(intent_hash = ?handle.intent_hash, "bump halted at approval envelope");
            return Ok(());
        }
        let Some(approval) = self.bump_approval(handle, bumped, now).await? else {
            return Ok(()); // policy revoked or tightened below the bump -> leave the tx
        };

        let tx = signing::build_tx(&handle.intent, handle.nonce, current.gas_limit, bumped);
        let signed =
            signing::sign_encode(&*self.signer, tx, handle.intent_hash, &approval, now).await?;

        // Escalation is one-way and persisted below, so recovery re-broadcasts publicly
        // rather than silently re-hiding a tx whose route gave up on staying private.
        if let SubmissionRoute::Private(route) = &handle.submission.route
            && let Escalation::PublicAfter { cycles } = route.escalation()
            && handle.broadcasts.len() >= *cycles as usize
        {
            warn!(intent_hash = ?handle.intent_hash, cycles = *cycles, "escalating stuck private tx to public mempool");
            handle.submission.route = SubmissionRoute::Public;
        }
        match self
            .submission
            .submit(signed.rlp.clone(), &handle.submission)
            .await
        {
            Ok(_) => {}
            // Already in the mempool (a prior round's replacement): record it, not an error.
            Err(e) if e.is_already_accepted() => {}
            Err(e) => return Err(e.into()),
        }

        handle.signed = signed.rlp;
        handle.broadcasts.push(signed.hash);
        handle.last_broadcast_at = now;
        self.state_store.put_handle(handle).await?;
        self.approvals.lock().insert(handle.id, approval);
        warn!(intent_hash = ?handle.intent_hash, nonce = handle.nonce, "bumped fees (RBF)");
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
        // `Cancel` default-allows a self-send; `Transaction` doesn't, so a stuck cancel
        // would wedge on the wrong shape.
        let intent = handle.intent.clone();
        let request = if intent.is_self_send() {
            SigningRequest::Cancel(intent)
        } else {
            SigningRequest::Transaction(intent)
        };
        match self.policy.evaluate(&request).await? {
            Decision::Allow(approval) => Ok(admits(&approval).then_some(approval)),
            Decision::Deny(_) => Ok(None),
        }
    }
}

/// A managed-relay handle whose task has not yet produced an on-chain hash — advanced by polling
/// the relay, not by reading the chain. Once its hash is recorded it is a normal chain-tracked tx.
fn is_awaiting_relay(handle: &TxHandle) -> bool {
    handle.broadcasts.is_empty() && handle.meta.as_ref().is_some_and(|meta| meta.task.is_some())
}

/// Emit a lifecycle transition at the right level: a terminal outcome is a milestone (INFO), an
/// intermediate advance is mechanics (DEBUG). `_prev` is underscored so it stays warning-free
/// when the obs macros compile to no-ops (`--no-default-features`).
fn log_transition(handle: &TxHandle, _prev: &TxStatus) {
    if handle.status.is_terminal() {
        info!(intent_hash = ?handle.intent_hash, from = ?_prev, to = ?handle.status, "transaction settled");
    } else {
        debug!(intent_hash = ?handle.intent_hash, from = ?_prev, to = ?handle.status, "status advanced");
    }
}

/// The execution outcome for a mined receipt, honoring gasless confirm-safety: a meta-tx whose
/// *outer* receipt succeeded still `Reverted`s unless the forwarder's `ExecutedForwardRequest`
/// proves the *inner* call also ran — a mined `execute()` is not evidence the user's intent
/// executed (H's no-false-`Confirmed` ethic, applied to meta-transactions).
fn outcome_of(receipt: &TransactionReceipt, handle: &TxHandle) -> Outcome {
    let inner_ok = match &handle.meta {
        // Self-relay: we submitted an OZ `execute()`, so the outer receipt alone is not proof —
        // decode the forwarder's `ExecutedForwardRequest` to confirm the *inner* call ran.
        Some(meta) if meta.task.is_none() => meta.inner_succeeded(receipt.logs()),
        // Managed relay (Gelato, `task = Some`) or a plain tx: no OZ event to decode. Gelato only
        // surfaced this hash on an `ExecSuccess` verdict, so the recorded receipt's own success is
        // the honest signal (a reverted receipt still fails below); a plain tx has no inner call.
        _ => true,
    };
    match receipt.status() && inner_ok {
        true => Outcome::Executed,
        false => Outcome::Reverted,
    }
}

/// Operational failures the executor surfaces from the ports it drives. Its own type
/// (not the pipeline's) — tracking and one-shot send are separate concerns.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutorError {
    /// An RPC call failed.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Fee estimation/bumping failed.
    #[error(transparent)]
    Gas(#[from] GasOracleError),
    /// Policy evaluation failed operationally (fail-closed).
    #[error(transparent)]
    Policy(#[from] PolicyEngineError),
    /// Nonce allocation/reconciliation failed.
    #[error(transparent)]
    Nonce(#[from] NonceManagerError),
    /// Signing failed.
    #[error(transparent)]
    Signer(#[from] SignerError),
    /// A durable-store operation failed.
    #[error(transparent)]
    Store(#[from] StateStoreError),
    /// Broadcasting the transaction failed.
    #[error(transparent)]
    Submission(#[from] SubmissionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::{Flashbots, Protect, RelayError, SubmissionOpts, TaskId};
    use crate::core::wallet::{GasEnvelope, MetaContext};
    use crate::testutils::{
        Harness, MockClock, MockGas, MockPolicy, MockRpc, MockStore, MockSubmit, Submit,
        estimation, handle, receipt, receipt_unanchored, signed_legacy,
    };
    use alloy_primitives::{B256, Bytes, U256};
    use async_trait::async_trait;

    // --- Private routing ---

    fn private_bump_harness(submit: Arc<MockSubmit>, store: Arc<MockStore>) -> AccountExecutor {
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 4,
                ..Default::default()
            }))
            .gas(Arc::new(MockGas {
                bump: Some(estimation(200, 1)),
                ..Default::default()
            }))
            .submit(submit)
            .clock(Arc::new(MockClock(1000)))
            .store(store)
            .bump_timeout(0)
            .executor()
    }

    #[tokio::test]
    async fn bump_stays_on_the_private_route() {
        // A `StayPrivate` tx that hasn't landed must re-broadcast privately — never leak
        // to the public mempool — and its persisted route must be untouched.
        let submit = Arc::new(MockSubmit::default());
        let store = Arc::new(MockStore::default());
        let exec = private_bump_harness(submit.clone(), store.clone());
        let opts: SubmissionOpts = Protect::mev_blocker(Escalation::StayPrivate).into();
        let mut h = handle(4, TxStatus::Sent);
        h.submission = opts.clone();
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, GasEnvelope::DEFAULT, u64::MAX),
        );
        exec.bump(&mut h, 1000).await.unwrap();
        assert_eq!(*submit.routes.lock(), vec![opts.route.clone()]);
        assert_eq!(store.all()[0].submission.route, opts.route);
    }

    #[tokio::test]
    async fn bump_escalates_private_to_public_at_threshold() {
        // `PublicAfter { cycles: 1 }` with one prior broadcast escalates on this bump: the
        // send goes public and the route rewrite is persisted (so recovery won't re-hide it).
        let submit = Arc::new(MockSubmit::default());
        let store = Arc::new(MockStore::default());
        let exec = private_bump_harness(submit.clone(), store.clone());
        let mut h = handle(4, TxStatus::Sent); // broadcasts.len() == 1
        h.submission = Flashbots::new(Escalation::PublicAfter { cycles: 1 }).into();
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, GasEnvelope::DEFAULT, u64::MAX),
        );
        exec.bump(&mut h, 1000).await.unwrap();
        assert_eq!(*submit.routes.lock(), vec![SubmissionRoute::Public]);
        assert_eq!(store.all()[0].submission.route, SubmissionRoute::Public);
    }

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

    #[tokio::test]
    async fn recover_rebroadcasts_only_pending_and_sent_across_multiple_inflight_handles() {
        // recover()'s `matches!(status, Pending | Sent)` guard rebroadcasts exactly the
        // live handles; Mined/Replacing are Confirm's job (rebroadcasting them only earns
        // a nonce-too-low). Distinct signed bytes per nonce make the set falsifiable.
        let store = Arc::new(MockStore::default());
        let h4 = handle(4, TxStatus::Sent);
        let h5 = handle(5, TxStatus::Pending);
        store.put_handle(&h4).await.unwrap();
        store.put_handle(&h5).await.unwrap();
        store
            .put_handle(&handle(
                6,
                TxStatus::Mined {
                    block: 8,
                    block_hash: B256::repeat_byte(1),
                },
            ))
            .await
            .unwrap();
        store
            .put_handle(&handle(7, TxStatus::Replacing { since_block: 8 }))
            .await
            .unwrap();
        let submit = Arc::new(MockSubmit::default());
        let exec = Harness::default()
            .submit(submit.clone())
            .store(store.clone())
            .executor();

        exec.recover().await.unwrap();

        let seen = submit.seen.lock();
        assert_eq!(seen.len(), 2); // only the two live handles
        assert!(seen.contains(&h4.signed));
        assert!(seen.contains(&h5.signed));
    }

    #[tokio::test]
    async fn recover_swallows_a_per_handle_submit_failure_and_still_attempts_later_handles() {
        // The `let _ =` on the per-handle submit result must not abort recovery for the
        // rest. Every submit errors (non-transient), but the mock records each attempt.
        let store = Arc::new(MockStore::default());
        let h4 = handle(4, TxStatus::Sent);
        let h5 = handle(5, TxStatus::Sent);
        store.put_handle(&h4).await.unwrap();
        store.put_handle(&h5).await.unwrap();
        let submit = Arc::new(MockSubmit {
            outcome: Submit::Deterministic,
            ..Default::default()
        });
        let exec = Harness::default()
            .submit(submit.clone())
            .store(store.clone())
            .executor();

        assert!(exec.recover().await.is_ok()); // the first handle's error did not abort
        let seen = submit.seen.lock();
        assert_eq!(seen.len(), 2); // both attempted despite the first erroring
        assert!(seen.contains(&h4.signed));
        assert!(seen.contains(&h5.signed));
    }

    #[tokio::test]
    async fn terminal_handles_are_readable_after_restart_but_not_rebroadcast() {
        // Terminal handles are excluded from the pending set (not re-tracked); only the
        // live Sent handle is rebroadcast on recovery. Fails if any terminal variant leaks.
        let store = Arc::new(MockStore::default());
        store
            .put_handle(&handle(4, TxStatus::Confirmed { block: 8 }))
            .await
            .unwrap();
        store
            .put_handle(&handle(5, TxStatus::Failed { reason: "x".into() }))
            .await
            .unwrap();
        store
            .put_handle(&handle(6, TxStatus::Replaced))
            .await
            .unwrap();
        let live = handle(7, TxStatus::Sent);
        store.put_handle(&live).await.unwrap();
        let submit = Arc::new(MockSubmit::default());
        let exec = Harness::default()
            .submit(submit.clone())
            .store(store.clone())
            .executor();

        exec.recover().await.unwrap();

        let seen = submit.seen.lock();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], live.signed);
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

    #[test]
    fn gasless_outer_success_does_not_confirm_a_reverted_inner_call() {
        // The J invariant at the shell: a mined outer `execute()` (receipt status = 1) is *not*
        // evidence the user's inner call ran. Without a matching `ExecutedForwardRequest`, a
        // meta-tx outcome is `Reverted` (→ `Failed` at the FSM), never `Executed`.
        let mined_ok = receipt(true, 8, B256::ZERO); // outer tx OK, no forwarder event
        let plain = handle(4, TxStatus::Sent);
        assert_eq!(outcome_of(&mined_ok, &plain), Outcome::Executed);

        let mut meta = handle(4, TxStatus::Sent);
        meta.meta = Some(MetaContext {
            forwarder: Address::ZERO,
            signer: Address::ZERO,
            nonce: U256::ZERO,
            task: None,
        });
        assert_eq!(outcome_of(&mined_ok, &meta), Outcome::Reverted);

        // A reverted outer tx is `Reverted` regardless of `meta`.
        assert_eq!(
            outcome_of(&receipt(false, 8, B256::ZERO), &plain),
            Outcome::Reverted
        );
    }

    #[test]
    fn gelato_task_handle_trusts_the_relay_verdict_not_the_oz_event() {
        // A managed-relay (Gelato) handle records its hash only after an `ExecSuccess` verdict, so
        // a successful outer receipt IS the honest inclusion signal — unlike self-relay, no OZ
        // `ExecutedForwardRequest` is decoded (Gelato's forwarder emits a different event). This is
        // the branch that would falsely `Failed` every Gelato tx if it reused the self-relay decode.
        let mined_ok = receipt(true, 8, B256::ZERO);
        let mut gelato = handle(4, TxStatus::Sent);
        gelato.meta = Some(MetaContext::for_gelato_task(
            Address::ZERO,
            Address::ZERO,
            U256::ZERO,
            TaskId::new("t1"),
        ));
        assert_eq!(outcome_of(&mined_ok, &gelato), Outcome::Executed);
        // A reverted outer receipt still fails — the task verdict never rescues a bad receipt.
        assert_eq!(
            outcome_of(&receipt(false, 8, B256::ZERO), &gelato),
            Outcome::Reverted
        );
    }

    /// A stub managed relay returning one canned status per poll.
    struct MockRelay(RelayStatus);

    #[async_trait]
    impl Relay for MockRelay {
        async fn poll(&self, _task: &TaskId) -> Result<RelayStatus, RelayError> {
            Ok(self.0.clone())
        }
    }

    /// A task-pending Gelato handle: no signed bytes (the relay submits) and no on-chain hash yet.
    fn task_handle() -> TxHandle {
        let mut h = handle(0, TxStatus::Sent);
        h.signed = Bytes::new();
        h.broadcasts = vec![];
        h.meta = Some(MetaContext::for_gelato_task(
            Address::ZERO,
            h.account,
            U256::ZERO,
            TaskId::new("task-1"),
        ));
        h
    }

    #[tokio::test]
    async fn task_handle_records_the_hash_on_relay_inclusion() {
        // The poll branch: a managed-relay handle with no hash is advanced by polling, not by the
        // chain. An `Included` verdict records the on-chain hash (and stays `Sent`) so the *next*
        // confirm cycle anchors and depth-confirms it exactly like any other tx.
        let store = Arc::new(MockStore::default());
        store.put_handle(&task_handle()).await.unwrap();
        let hash = B256::repeat_byte(9);
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 0,
                block_number: 10,
                ..Default::default()
            }))
            .store(store.clone())
            .executor()
            .with_relay(Arc::new(MockRelay(RelayStatus::Included(hash))))
            .confirm()
            .await
            .unwrap();
        let got = &store.all()[0];
        assert_eq!(got.broadcasts, vec![hash], "the included hash is recorded");
        assert_eq!(
            got.status,
            TxStatus::Sent,
            "handoff to the chain-confirm path"
        );
    }

    #[tokio::test]
    async fn task_handle_settles_failed_on_relay_drop() {
        // A relay `Failed` verdict (ExecReverted / Cancelled) is terminal — the handle settles
        // `Failed` rather than polling forever.
        let store = Arc::new(MockStore::default());
        store.put_handle(&task_handle()).await.unwrap();
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 0,
                block_number: 10,
                ..Default::default()
            }))
            .store(store.clone())
            .executor()
            .with_relay(Arc::new(MockRelay(RelayStatus::Failed("cancelled".into()))))
            .confirm()
            .await
            .unwrap();
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
    async fn regressed_head_between_cycles_skips_confirm_and_makes_no_transition() {
        // A lagging/failover node serving an older head must short-circuit read_cycle
        // *before* the nonce reset (and thus before any transition). One executor (the
        // guard is stateful in last_latest) drives three cycles: 100 advances -> 90
        // regressed (skip) -> 95 still below 100 (skip — proving the skip didn't corrupt
        // last_latest down to 90). reset() runs exactly once (cycle 1); counting it is the
        // decisive proof the two regressed cycles never entered the confirm body. Distinct
        // from inconsistent_view_skips_the_cycle (which trips the finalized > latest guard).
        use crate::testutils::{MockNonce, shared_log};

        let log = shared_log();
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        let exec = Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 4, // nonce not consumed -> cycle 1 is a Pending no-op
                block_numbers: Mutex::new([100, 90, 95].into()),
                ..Default::default()
            }))
            .nonce(Arc::new(MockNonce {
                next: 0,
                log: log.clone(),
            }))
            .store(store.clone())
            .confirmations(2)
            .executor();

        for _ in 0..3 {
            exec.confirm().await.unwrap();
        }

        assert_eq!(store.all()[0].status, TxStatus::Sent); // no transition, ever
        let resets = log.lock().iter().filter(|e| **e == "reset").count();
        assert_eq!(resets, 1); // only cycle 1 ran the body; both regressed cycles skipped
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

    /// Escalate a stuck (Sent, nonce 4, wide DEFAULT envelope) handle whose bump has **no
    /// cached approval**, so `bump_approval` re-evaluates `policy`. Returns the store for
    /// outcome assertions. The wide handle envelope keeps the per-intent hard cap out of
    /// the way, so the only gate is the fresh policy decision.
    async fn escalate_with_fresh_policy(policy: Arc<MockPolicy>) -> Arc<MockStore> {
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
            .policy(policy)
            .clock(Arc::new(MockClock(1000)))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
        exec.escalate().await.unwrap();
        store
    }

    /// Escalate a stuck (Sent, nonce 4) handle whose persisted `signed` is `signed`. A
    /// working gas oracle (bump 200/1) and allowing policy are wired, so the tx staying
    /// unbumped can only mean `decode_fees` rejected the body. Returns store + policy.
    async fn escalate_with_signed(signed: Bytes) -> (Arc<MockStore>, Arc<MockPolicy>) {
        let policy = Arc::new(MockPolicy::default());
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
            .policy(policy.clone())
            .clock(Arc::new(MockClock(1000)))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        let mut h = handle(4, TxStatus::Sent);
        h.signed = signed;
        store.put_handle(&h).await.unwrap();
        exec.escalate().await.unwrap();
        (store, policy)
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
    async fn bump_records_broadcast_when_already_known_and_persists_new_hash() {
        // "already known" == the replacement is already in the mempool -> record it as a
        // broadcast (so Confirm can match its receipt), not an error that drops it. The
        // shared mutation block must also advance `signed` to the bumped body, so a later
        // recover() rebroadcasts the replacement rather than the stale original.
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
        // signed advanced to the bumped 200/1 body, not the original 100/1.
        let fees = signing::decode_fees(&store.all()[0].signed)
            .expect("bumped 1559 body")
            .fees;
        assert_eq!(fees.max_fee_per_gas, 200);
        assert_eq!(fees.max_priority_fee_per_gas, 1);
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

    #[tokio::test]
    async fn bump_denied_by_fresh_policy_leaves_the_tx() {
        // Policy revoked between send and bump: with no cached approval the bump
        // re-evaluates, gets Deny, and leaves the tx — no broadcast, no cycle error.
        let policy = Arc::new(MockPolicy {
            allow: false,
            ..Default::default()
        });
        let store = escalate_with_fresh_policy(policy.clone()).await;
        assert_eq!(store.all()[0].broadcasts.len(), 1);
        assert_eq!(store.all()[0].status, TxStatus::Sent);
        // calls==1 proves control reached the Deny arm, not an earlier envelope/ceiling
        // short-circuit (both of which leave calls==0).
        assert_eq!(*policy.calls.lock(), 1);
    }

    #[tokio::test]
    async fn bump_denied_when_refreshed_envelope_no_longer_admits_the_bump() {
        // Policy re-approves but returns a *tightened* envelope that no longer admits the
        // bumped fees -> stop, not broadcast (the false arm of `then_some`). The handle's
        // own envelope stays wide, so the fresh approval's envelope is the sole rejection.
        let policy = Arc::new(MockPolicy {
            allow: true,
            envelope: GasEnvelope {
                max_fee_cap: 150,
                max_priority_cap: 150,
            },
            valid_until: u64::MAX,
            ..Default::default()
        });
        let store = escalate_with_fresh_policy(policy.clone()).await;
        assert_eq!(store.all()[0].broadcasts.len(), 1);
        assert_eq!(store.all()[0].status, TxStatus::Sent);
        // calls==1 distinguishes this from bump_beyond_approved_envelope_stops (calls==0,
        // stopped earlier at the per-intent hard cap).
        assert_eq!(*policy.calls.lock(), 1);
    }

    #[tokio::test]
    async fn bump_exactly_at_envelope_ceiling_is_admitted() {
        // A bump landing exactly on the envelope caps is admitted (inclusive `<=` in
        // GasEnvelope::admits) — not stranded one wei short. The cached approval carries
        // the same caps and is reused (calls==0), so the boundary is the only site tested.
        let (exec, store, policy) = send_setup(Some(estimation(200, 1)));
        let caps = GasEnvelope {
            max_fee_cap: 200,
            max_priority_cap: 1,
        };
        seed_and_track(&exec, &store, caps).await;
        exec.escalate().await.unwrap();
        assert_eq!(store.all()[0].broadcasts.len(), 2); // bumped
        assert_eq!(*policy.calls.lock(), 0); // cached approval reused
    }

    #[tokio::test]
    async fn decode_fees_leaves_a_non_1559_signed_tx_unbumped() {
        // A cleanly-decoding but non-1559 (legacy) body -> decode_fees returns None, so the
        // bump bails before the oracle/policy. A working oracle is wired, so broadcasts
        // staying at 1 can only mean decode_fees rejected the body.
        let signed = signed_legacy(4);
        let (store, policy) = escalate_with_signed(signed.clone()).await;
        let h = &store.all()[0];
        assert_eq!(h.broadcasts.len(), 1);
        assert_eq!(h.signed, signed); // untouched
        assert_eq!(h.status, TxStatus::Sent);
        assert_eq!(*policy.calls.lock(), 0); // never reached policy
    }

    #[tokio::test]
    async fn decode_fees_leaves_a_handle_with_undecodable_signed_bytes() {
        // Garbled persisted bytes (corrupt WAL): a bad EIP-2718 type byte makes decode_2718
        // Err, so decode_fees returns None via `.ok()?` — the bump short-circuits, no crash.
        let signed = Bytes::from(vec![0xff, 0x00, 0x01]);
        let (store, policy) = escalate_with_signed(signed.clone()).await;
        let h = &store.all()[0];
        assert_eq!(h.broadcasts.len(), 1);
        assert_eq!(h.signed, signed);
        assert_eq!(h.status, TxStatus::Sent);
        assert_eq!(*policy.calls.lock(), 0);
    }

    #[tokio::test]
    async fn bump_transient_submit_error_aborts_without_advancing_broadcasts() {
        // A non-already-accepted submit error must return before the mutation block,
        // recording nothing (check submit *before* recording the broadcast). Driven via
        // the direct bump() since escalate() swallows the per-handle Err.
        let submit = Arc::new(MockSubmit {
            outcome: Submit::Transient,
            ..Default::default()
        });
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
            .submit(submit.clone())
            .clock(Arc::new(MockClock(1000)))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        let mut h = handle(4, TxStatus::Sent);
        let original = h.signed.clone();
        store.put_handle(&h).await.unwrap();
        exec.track(
            h.id,
            PolicyApproval::mint(h.intent_hash, GasEnvelope::DEFAULT, u64::MAX),
        );
        let result = exec.bump(&mut h, 1000).await;
        assert!(matches!(result, Err(ExecutorError::Submission(_))));
        assert_eq!(submit.seen.lock().len(), 1); // the bump did attempt a broadcast
        // The mutation block was skipped: no phantom hash, signed still the original.
        assert_eq!(store.all()[0].broadcasts.len(), 1);
        assert_eq!(store.all()[0].signed, original);
    }

    #[tokio::test]
    async fn repeated_bumps_across_ticks_append_broadcasts_at_the_same_nonce() {
        // Each stuck tick appends a broadcast (never overwrites) at a stable nonce/id —
        // the OZ/thirdweb stable-id contract; a bump never advances the lifecycle.
        let (exec, store, policy) = send_setup(Some(estimation(200, 1)));
        seed_and_track(&exec, &store, GasEnvelope::DEFAULT).await;
        for _ in 0..3 {
            exec.escalate().await.unwrap();
        }
        let h = &store.all()[0];
        assert_eq!(h.broadcasts.len(), 4); // original + one appended per tick
        assert_eq!(h.nonce, 4);
        assert_eq!(h.id, handle(4, TxStatus::Sent).id); // id stable across bumps
        assert_eq!(h.status, TxStatus::Sent);
        assert_eq!(*policy.calls.lock(), 0); // cached approval reused every round
    }

    #[tokio::test]
    async fn newest_broadcast_receipt_wins_over_stale_older_hash() {
        // After an RBF bump, event_for scans broadcasts newest-first. Two cases pin it.
        use alloy_primitives::TxHash;
        use std::collections::HashMap;
        let h_old = TxHash::repeat_byte(0x11);
        let h_new = TxHash::repeat_byte(0x22);
        let canonical = B256::repeat_byte(0x88);

        // Case A (ordering): both broadcasts have receipts at different blocks; the newer
        // (block 8) must win over the older (block 6). A forward scan would not confirm 8.
        let store = Arc::new(MockStore::default());
        let mut h = handle(4, TxStatus::Sent);
        h.broadcasts = vec![h_old, h_new];
        store.put_handle(&h).await.unwrap();
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 5,
                block_number: 20,
                canonical: Some(canonical),
                receipts: HashMap::from([
                    (h_old, receipt(true, 6, B256::repeat_byte(0x66))),
                    (h_new, receipt(true, 8, canonical)),
                ]),
                ..Default::default()
            }))
            .store(store.clone())
            .confirmations(2)
            .executor()
            .confirm()
            .await
            .unwrap();
        assert_eq!(store.all()[0].status, TxStatus::Confirmed { block: 8 });

        // Case B (receipt beats nonce): our nonce is consumed (tx_count 5 > 4), which by
        // the nonce path alone reads as Replaced — but the bump's hash-anchored receipt
        // overrides that to a mined outcome.
        let store = Arc::new(MockStore::default());
        let mut h = handle(4, TxStatus::Sent);
        h.broadcasts = vec![h_old, h_new];
        store.put_handle(&h).await.unwrap();
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 5,
                block_number: 20,
                canonical: Some(canonical),
                receipts: HashMap::from([(h_new, receipt(true, 8, canonical))]),
                ..Default::default()
            }))
            .store(store.clone())
            .confirmations(2)
            .executor()
            .confirm()
            .await
            .unwrap();
        assert_eq!(store.all()[0].status, TxStatus::Confirmed { block: 8 });
    }

    #[tokio::test]
    async fn bump_then_original_mines_bump_receipt_is_ignored_original_wins() {
        // RBF doesn't guarantee the bump wins: the newest (bump) broadcast is receiptless,
        // so the newest-first scan must `continue` past its Ok(None) to the older mined
        // original — not early-return Unknown/Replacing on the first receiptless hash.
        use alloy_primitives::TxHash;
        use std::collections::HashMap;
        let h_orig = TxHash::repeat_byte(0x11);
        let h_bump = TxHash::repeat_byte(0x22);
        let canonical = B256::repeat_byte(0x88);
        let store = Arc::new(MockStore::default());
        let mut h = handle(4, TxStatus::Sent);
        h.broadcasts = vec![h_orig, h_bump];
        store.put_handle(&h).await.unwrap();
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 5,
                block_number: 10,
                canonical: Some(canonical),
                receipts: HashMap::from([(h_orig, receipt(true, 8, canonical))]),
                ..Default::default()
            }))
            .store(store.clone())
            .confirmations(2)
            .executor()
            .confirm()
            .await
            .unwrap();
        assert_eq!(store.all()[0].status, TxStatus::Confirmed { block: 8 });
    }

    #[tokio::test]
    async fn receipt_read_error_yields_unknown_holds_state() {
        // A transient receipt-RPC error must be a no-op (Unknown), never misread as "not
        // mined" — which would rewind a mined handle. The nonce is consumed (tx_count 5 >
        // 4), so the only thing preventing a state change is the Err->Unknown short-circuit.
        let h1 = B256::repeat_byte(1);
        let mined = TxStatus::Mined {
            block: 8,
            block_hash: h1,
        };

        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, mined.clone())).await.unwrap();
        Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 5,
                block_number: 20,
                receipt_err: true,
                ..Default::default()
            }))
            .store(store.clone())
            .confirmations(2)
            .executor()
            .confirm()
            .await
            .unwrap();
        assert_eq!(store.all()[0].status, mined); // unchanged

        // Distinctness: same view, a clean None read instead lets the nonce path apply
        // (nonce 4 < 5 -> Replaced), so the Err path is observably different from None.
        let store = Arc::new(MockStore::default());
        store.put_handle(&handle(4, mined.clone())).await.unwrap();
        run_confirm(&store, 5, 20, None, None, 2).await;
        assert_eq!(
            store.all()[0].status,
            TxStatus::Replacing { since_block: 20 }
        );
    }

    #[tokio::test]
    async fn stuck_cancel_bumps_via_cancel_request() {
        // Real engine, no rules: `Transaction` self-send default-denies, `Cancel` default-
        // allows. If `bump_approval` picks by shape, the RBF proceeds; a wrong pick wedges.
        use crate::adapters::SystemClock;
        use crate::adapters::policy::DefaultPolicyEngine;
        use crate::core::wallet::TxIntent;
        use alloy_primitives::{TxKind, U256};

        let account = Address::ZERO;
        let self_send = TxIntent {
            chain_id: 1,
            account,
            to: TxKind::Call(account),
            value: U256::ZERO,
            input: Bytes::new(),
            purpose: None,
        };
        let intent_hash = self_send.hash();
        let mut h = handle(4, TxStatus::Sent);
        h.intent = self_send;
        h.intent_hash = intent_hash;
        h.id = HandleId::new(intent_hash, 4);

        let store = Arc::new(MockStore::default());
        store.put_handle(&h).await.unwrap();
        let exec = Harness::default()
            .rpc(Arc::new(MockRpc {
                tx_count: 4,
                ..Default::default()
            }))
            .gas(Arc::new(MockGas {
                bump: Some(estimation(200, 1)),
                ..Default::default()
            }))
            .policy(Arc::new(DefaultPolicyEngine::new(
                vec![],
                Arc::new(SystemClock),
            )))
            .store(store.clone())
            .bump_timeout(0)
            .executor();
        exec.escalate().await.unwrap();

        assert_eq!(store.all()[0].broadcasts.len(), 2);
    }

    #[tokio::test]
    async fn receipt_missing_block_anchor_yields_unknown() {
        // A receipt with no block anchor (or only one of the two fields) is the
        // pending/partial shape: anchor()'s tuple-destructure guard makes it Unknown and
        // holds state. `canonical` is set to a hash that would confirm, so only the guard
        // prevents a transition.
        let mut half = receipt(true, 8, B256::repeat_byte(1));
        half.block_hash = None; // block_number Some, block_hash None
        for r in [receipt_unanchored(), half] {
            let store = Arc::new(MockStore::default());
            store.put_handle(&handle(4, TxStatus::Sent)).await.unwrap();
            run_confirm(&store, 4, 20, Some(r), Some(B256::repeat_byte(1)), 2).await;
            assert_eq!(store.all()[0].status, TxStatus::Sent);
        }
    }
}
