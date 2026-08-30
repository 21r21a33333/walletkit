//! `TransactionManager` — the one-shot send pipeline: it turns a [`TxIntent`] into
//! a broadcast transaction plus a persisted, queryable [`TxHandle`]. Tracking,
//! bumping, and reorg handling are the executor's job; this is the fixed-order build
//! path, reusing alloy for all tx mechanics.

use super::signing;
use crate::core::deps::{
    Clock, GasOracle, GasOracleError, NonceManager, NonceManagerError, PolicyEngine,
    PolicyEngineError, RelayError, RouteError, Rpc, RpcError, SignedRequest, Signer, SignerError,
    Simulated, StateStore, StateStoreError, SubmissionError, SubmissionOpts, SubmissionStrategy,
};
use crate::core::wallet::{
    Decision, ForwardRequest, ForwarderDomain, HandleId, IntentHash, MetaContext, PolicyApproval,
    PolicyRejection, SignatureEnvelope, SigningRequest, TxHandle, TxIntent, TxStatus,
    decode_forwarder_nonce, nonces_calldata,
};
use crate::error::WalletKitError;
use crate::obs::{debug, error, info, warn};
use alloy_dyn_abi::TypedData;
use alloy_eips::eip1559::Eip1559Estimation;
use alloy_primitives::{Address, Bytes, TxKind, U256, aliases::U48};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use std::sync::Arc;
use std::time::Duration;

/// Gas-limit buffer over `eth_estimateGas`. viem/ethers trust the raw estimate, but it
/// can underestimate for gas-forwarding / failure-swallowing contracts (geth #21746,
/// the 63/64 rule) and state drifts before inclusion; over-provisioning is ~free
/// (EIP-1559 refunds unused gas) while underestimating burns a reverted tx. Tunable
/// via [`TransactionManager::with_gas_buffer_pct`]. Percent.
const DEFAULT_GAS_BUFFER_PCT: u128 = 25;

/// Gas for a cancel self-send — the exact base tx cost (0-value, empty calldata), so it
/// needs no estimation (Yellow Paper G_transaction).
const CANCEL_GAS_LIMIT: u64 = 21_000;

/// Staged send pipeline: policy → nonce → gas → sign → submit → persist, plus cancel
/// and bump. Wraps the ports; the [`Wallet`](crate::Wallet) facade drives it.
pub struct TransactionManager {
    rpc: Arc<dyn Rpc>,
    gas_oracle: Arc<dyn GasOracle>,
    policy: Arc<dyn PolicyEngine>,
    nonce_manager: Arc<dyn NonceManager>,
    signer: Arc<dyn Signer>,
    submission: Arc<dyn SubmissionStrategy>,
    state_store: Arc<dyn StateStore>,
    clock: Arc<dyn Clock>,
    gas_buffer_pct: u128,
}

impl TransactionManager {
    /// Wire the pipeline from its ports (`gas_buffer_pct` pads the estimated gas limit).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc: Arc<dyn Rpc>,
        gas_oracle: Arc<dyn GasOracle>,
        policy: Arc<dyn PolicyEngine>,
        nonce_manager: Arc<dyn NonceManager>,
        signer: Arc<dyn Signer>,
        submission: Arc<dyn SubmissionStrategy>,
        state_store: Arc<dyn StateStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            rpc,
            gas_oracle,
            policy,
            nonce_manager,
            signer,
            submission,
            state_store,
            clock,
            gas_buffer_pct: DEFAULT_GAS_BUFFER_PCT,
        }
    }

    /// Override the gas-limit buffer (percent). Lower it for cost-sensitive,
    /// well-estimated txs; raise it for gas-forwarding contracts (see the const).
    pub fn with_gas_buffer_pct(mut self, pct: u128) -> Self {
        self.gas_buffer_pct = pct;
        self
    }

    /// Sign an EIP-191 `personal_sign` message through the policy gate. Blind signing is
    /// impossible: the message is `0x19`-prefixed and default-denied unless a rule allows it.
    /// `skip_all` is mandatory — only the safe `payload_hash` is recorded, never the message.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            name = "sign_message",
            level = "debug",
            skip_all,
            fields(payload_hash = %alloy_primitives::eip191_hash_message(message))
        )
    )]
    pub async fn sign_message(
        &self,
        message: &[u8],
    ) -> Result<SignatureEnvelope, TransactionManagerError> {
        let request = SigningRequest::Message(Bytes::copy_from_slice(message));
        let approval = self.authorize(&request).await?;
        Ok(self
            .signer
            .sign_message(message, &approval, self.clock.now_unix())
            .await?)
    }

    /// Sign EIP-712 typed data through the policy gate (domain `chainId` validated in the
    /// signer). A domain not on an allowlisted `verifyingContract` is default-denied.
    /// `skip_all` keeps the typed-data content out of telemetry; only the hash is recorded.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            name = "sign_typed_data",
            level = "debug",
            skip_all,
            fields(payload_hash = ?typed.eip712_signing_hash().ok())
        )
    )]
    pub async fn sign_typed_data(
        &self,
        typed: &TypedData,
    ) -> Result<SignatureEnvelope, TransactionManagerError> {
        let request = SigningRequest::TypedData(Box::new(typed.clone()));
        let approval = self.authorize(&request).await?;
        Ok(self
            .signer
            .sign_typed_data(typed, &approval, self.clock.now_unix())
            .await?)
    }

    /// Run the policy gate for a signing request, returning the minted approval or a denial.
    async fn authorize(
        &self,
        request: &SigningRequest,
    ) -> Result<PolicyApproval, TransactionManagerError> {
        match self.policy.evaluate(request).await? {
            Decision::Allow(approval) => Ok(approval),
            Decision::Deny(rejection) => Err(TransactionManagerError::Denied(rejection)),
        }
    }

    /// Estimate (also the pre-sign revert gate) → fees → policy → allocate → build →
    /// sign → persist → submit. A nonce is allocated only after policy Allow and
    /// released if any later step fails, so a denied or failed send never leaves a gap.
    pub async fn send(&self, intent: &TxIntent) -> Result<TxHandle, TransactionManagerError> {
        self.send_with(intent, &SubmissionOpts::default()).await
    }

    /// Like [`send`](Self::send) but routing the broadcast per `opts` (public mempool or a
    /// private relay). The route is validated up front and persisted on the handle so bumps
    /// and crash-recovery re-send on the original route.
    pub async fn send_with(
        &self,
        intent: &TxIntent,
        opts: &SubmissionOpts,
    ) -> Result<TxHandle, TransactionManagerError> {
        self.send_with_meta(intent, opts, None).await
    }

    /// The send pipeline, optionally stamping `meta` (a gasless forwarder `execute()`). A plain
    /// send passes `None`; the self-relay path passes the forwarder/signer/nonce so the
    /// relayer's executor honors the confirmation-safety decode. The account signing the tx is
    /// this manager's signer, so a gasless outer tx is sent by the *relayer* manager, whose
    /// signer is the relayer — no per-tx signer selection.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            name = "wallet.send",
            level = "info",
            skip_all,
            fields(intent_hash = ?intent.hash(), account = %intent.account, chain_id = intent.chain_id, gasless = meta.is_some())
        )
    )]
    pub(crate) async fn send_with_meta(
        &self,
        intent: &TxIntent,
        opts: &SubmissionOpts,
        meta: Option<MetaContext>,
    ) -> Result<TxHandle, TransactionManagerError> {
        // Fail fast (before any nonce or chain touch) if the strategy can't honor the route.
        if !self.submission.supports_route(&opts.route) {
            return Err(RouteError::RelayNotConfigured.into());
        }
        let account = intent.account;
        // The nonce is allocated for `account`; signing with a different key would put
        // it on the wrong sender. Fail before touching the chain or a nonce.
        let signer = self.signer.address();
        if signer != account {
            return Err(TransactionManagerError::AccountMismatch {
                intent: account,
                signer,
            });
        }
        let request = TransactionRequest {
            from: Some(account),
            to: Some(intent.to),
            value: Some(intent.value),
            input: TransactionInput::new(intent.input.clone()),
            ..Default::default()
        };

        // estimate_gas executes the tx, so it doubles as the pre-sign revert gate: a
        // deterministic failure means it would revert (a transient one is retryable).
        let gas_limit = match self.rpc.estimate_gas(&request).await {
            Ok(gas) => self.buffered_gas(gas),
            Err(RpcError::Call {
                transient: false,
                message,
            }) => return Err(TransactionManagerError::SimulationRejected { reason: message }),
            Err(e) => return Err(e.into()),
        };
        let fees = self.gas_oracle.estimate().await?;

        let request = SigningRequest::Transaction(intent.clone());
        let approval = match self.policy.evaluate(&request).await? {
            Decision::Allow(approval) => approval,
            Decision::Deny(rejection) => return Err(TransactionManagerError::Denied(rejection)),
        };

        let nonce = self.nonce_manager.allocate(account).await?;
        debug!(nonce, "nonce allocated");
        // `build_sign_submit` owns the nonce lifecycle: it recycles the nonce only when
        // nothing was broadcast, so a live tx's nonce is never freed for reuse.
        self.build_sign_submit(intent, gas_limit, fees, nonce, approval, opts, meta)
            .await
    }

    /// The account this manager signs transactions for (its signer's address).
    pub(crate) fn account(&self) -> Address {
        self.signer.address()
    }

    /// Read the forwarder's current nonce for `owner` (sequential replay protection). A revert
    /// or an undecodable return means the address is not a conforming `ERC2771Forwarder`.
    pub(crate) async fn forwarder_nonce(
        &self,
        forwarder: Address,
        owner: Address,
    ) -> Result<U256, RelayError> {
        let request = TransactionRequest {
            to: Some(TxKind::Call(forwarder)),
            input: TransactionInput::new(nonces_calldata(owner)),
            ..Default::default()
        };
        match self.rpc.call(&request).await? {
            Simulated::Returned(bytes) => {
                decode_forwarder_nonce(&bytes).ok_or_else(|| RelayError::Forwarder {
                    message: "nonces() returned undecodable data — not an ERC2771Forwarder".into(),
                })
            }
            Simulated::Reverted(_) => Err(RelayError::Forwarder {
                message: "nonces() reverted — the address is not an ERC2771Forwarder".into(),
            }),
        }
    }

    /// Build the user's [`ForwardRequest`] for `intent` bound to `forwarder`, and sign it through
    /// the **existing** policy gate as EIP-712 typed data. The gasless analog of building and
    /// signing a tx: the user authorizes (and pays nothing), but never sends — a relayer does.
    /// `deadline` is the validity window from now. Returns `WalletKitError` because it fuses two
    /// domains: the forwarder read ([`RelayError`]) and the signing gate ([`TransactionManagerError`]).
    pub(crate) async fn build_and_sign_forward_request(
        &self,
        intent: &TxIntent,
        forwarder: Address,
        domain: &ForwarderDomain,
        deadline: Duration,
    ) -> Result<SignedRequest, WalletKitError> {
        let owner = intent.account;
        let target = match intent.to {
            TxKind::Call(to) => to,
            TxKind::Create => {
                return Err(RelayError::Rejected {
                    message: "contract creation cannot be relayed via ERC-2771".into(),
                }
                .into());
            }
        };
        let nonce = self.forwarder_nonce(forwarder, owner).await?;
        // The inner call's gas, estimated as the user calling the target directly (the same
        // `msg.sender` the forwarder reproduces). A deterministic revert ⇒ the request would fail.
        let inner = TransactionRequest {
            from: Some(owner),
            to: Some(intent.to),
            value: Some(intent.value),
            input: TransactionInput::new(intent.input.clone()),
            ..Default::default()
        };
        let gas = match self.rpc.estimate_gas(&inner).await {
            Ok(gas) => gas,
            Err(RpcError::Call {
                transient: false,
                message,
            }) => return Err(WalletKitError::Simulation { reason: message }),
            Err(e) => return Err(RelayError::from(e).into()),
        };
        // uint48 deadline; clamp so a far-future window can never panic the conversion.
        const U48_MAX: u64 = (1u64 << 48) - 1;
        let deadline = self
            .clock
            .now_unix()
            .saturating_add(deadline.as_secs())
            .min(U48_MAX);
        let request = ForwardRequest {
            from: owner,
            to: target,
            value: intent.value,
            gas: U256::from(gas),
            nonce,
            deadline: U48::from(deadline),
            data: intent.input.clone(),
        };
        let typed = request.typed_data(forwarder, intent.chain_id, domain);
        let signature = self.sign_typed_data(&typed).await?;
        Ok(SignedRequest::new(
            request,
            signature,
            forwarder,
            intent.chain_id,
        ))
    }

    /// Cancel a pending tx: a policy-gated 0-value self-send at its nonce (RBF). Errors if
    /// the tx already settled; the original settles as `Dropped` once the cancel mines.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(name = "wallet.cancel", level = "info", skip_all, fields(id = ?id))
    )]
    pub async fn cancel(&self, id: HandleId) -> Result<TxHandle, TransactionManagerError> {
        let mut target = self
            .state_store
            .handle(id)
            .await?
            .ok_or(TransactionManagerError::UnknownHandle)?;
        if target.status.is_terminal() {
            return Err(TransactionManagerError::CancelTerminal);
        }
        let account = target.account;
        let intent = TxIntent {
            chain_id: target.intent.chain_id,
            account,
            to: TxKind::Call(account),
            value: U256::ZERO,
            input: Bytes::new(),
            purpose: None,
        };
        let intent_hash = intent.hash();
        let approval = self
            .authorize(&SigningRequest::Cancel(intent.clone()))
            .await?;

        // Persist cancelled=true before broadcasting: any consumption of this nonce settles
        // the original Dropped, regardless of what the submit does.
        target.cancelled = true;
        self.state_store.put_handle(&target).await?;

        let now = self.clock.now_unix();
        let basis = fee_basis(&target.signed)?;
        let signed = match self
            .broadcast_cancel_with_repricing(&target, &intent, intent_hash, basis, &approval, now)
            .await
        {
            Ok(signed) => signed,
            // The cancel never broadcast — un-poison the target so a later foreign displacement
            // settles it `Replaced` (refillable), not a spurious `Dropped`.
            Err(e) => {
                target.cancelled = false;
                let _ = self.state_store.put_handle(&target).await;
                return Err(e);
            }
        };

        let cancel = TxHandle {
            id: HandleId::new(intent_hash, target.nonce),
            account,
            intent,
            intent_hash,
            nonce: target.nonce,
            status: TxStatus::Sent,
            envelope: approval.gas_envelope(),
            signed: signed.rlp,
            broadcasts: vec![signed.hash],
            last_broadcast_at: now,
            cancelled: false,
            // The cancel rides the same route as the tx it replaces (a private tx's cancel
            // stays private).
            submission: target.submission.clone(),
            // A cancel is a plain self-send, never a gasless meta-tx.
            meta: None,
        };
        self.state_store.put_handle(&cancel).await?;
        info!(nonce = target.nonce, "cancel submitted");
        Ok(cancel)
    }

    /// Sign and submit the cancel at `basis`; on `replacement transaction underpriced`
    /// (target re-priced concurrently) re-fetch its fees and resend once. `already known`
    /// / `nonce too low` count as success (the tx is already in the pool).
    async fn broadcast_cancel_with_repricing(
        &self,
        target: &TxHandle,
        intent: &TxIntent,
        intent_hash: IntentHash,
        mut basis: Eip1559Estimation,
        approval: &PolicyApproval,
        now: u64,
    ) -> Result<signing::SignedTx, TransactionManagerError> {
        let mut attempts = 0u8;
        loop {
            let fees = self.gas_oracle.bump(basis).await?;
            let tx = signing::build_tx(intent, target.nonce, CANCEL_GAS_LIMIT, fees);
            let signed =
                signing::sign_encode(&*self.signer, tx, intent_hash, approval, now).await?;
            match self
                .submission
                .submit(signed.rlp.clone(), &target.submission)
                .await
            {
                Ok(_) => return Ok(signed),
                Err(e) if e.is_already_accepted() => return Ok(signed),
                Err(e) if e.is_underpriced() && attempts == 0 => {
                    attempts += 1;
                    let fresh = self
                        .state_store
                        .handle(target.id)
                        .await?
                        .ok_or(TransactionManagerError::UnknownHandle)?;
                    basis = fee_basis(&fresh.signed)?;
                    warn!(
                        nonce = target.nonce,
                        "cancel underpriced; re-basing over the target"
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_sign_submit(
        &self,
        intent: &TxIntent,
        gas_limit: u64,
        fees: Eip1559Estimation,
        nonce: u64,
        approval: PolicyApproval,
        opts: &SubmissionOpts,
        meta: Option<MetaContext>,
    ) -> Result<TxHandle, TransactionManagerError> {
        let account = intent.account;
        let intent_hash = intent.hash();
        let now = self.clock.now_unix();
        let tx = signing::build_tx(intent, nonce, gas_limit, fees);
        // Pre-broadcast failure (sign): nothing was sent, so recycle the nonce.
        let signed =
            match signing::sign_encode(&*self.signer, tx, intent_hash, &approval, now).await {
                Ok(out) => out,
                Err(e) => {
                    let _ = self.nonce_manager.release(account, nonce).await;
                    return Err(e.into());
                }
            };

        let mut handle = TxHandle {
            id: HandleId::new(intent_hash, nonce),
            account,
            intent: intent.clone(),
            intent_hash,
            nonce,
            status: TxStatus::Pending,
            // The originally-approved ceiling; a later bump must never exceed it.
            envelope: approval.gas_envelope(),
            signed: signed.rlp.clone(),
            broadcasts: vec![signed.hash],
            last_broadcast_at: now,
            cancelled: false,
            submission: opts.clone(),
            // Present only on the self-relay path — drives the relayer executor's confirm decode.
            meta,
        };
        // Persist the signed tx before broadcast (WAL). A pre-broadcast persist failure
        // means nothing was sent -> recycle the nonce.
        if let Err(e) = self.state_store.put_handle(&handle).await {
            let _ = self.nonce_manager.release(account, nonce).await;
            return Err(e.into());
        }

        match self.submission.submit(signed.rlp, opts).await {
            Ok(_) => {}
            // Transient (may be in flight) or already-accepted ("already known"/"nonce
            // too low" -> already sent/mined): assume sent — keep the nonce reserved
            // (releasing could reuse a live nonce -> double spend) and let recover()/
            // confirm() settle it.
            Err(e) if e.is_transient() || e.is_already_accepted() => {
                handle.status = TxStatus::Sent;
                let _ = self.state_store.put_handle(&handle).await;
                warn!(nonce, "submission indeterminate; assuming sent");
                return Ok(handle);
            }
            // Deterministic reject: definitely not broadcast -> terminalize + recycle.
            Err(e) => {
                handle.status = TxStatus::Failed {
                    reason: e.to_string(),
                };
                let _ = self.state_store.put_handle(&handle).await;
                let _ = self.nonce_manager.release(account, nonce).await;
                error!(error = %e, nonce, "submission rejected; nonce recycled");
                return Err(e.into());
            }
        }

        // Broadcast confirmed. Reflect Sent; a persist failure here must NOT release the
        // nonce — the tx is live, and freeing its nonce would enable reuse.
        handle.status = TxStatus::Sent;
        let _ = self.state_store.put_handle(&handle).await;
        info!(tx_hash = ?signed.hash, nonce, "transaction submitted");
        Ok(handle)
    }

    fn buffered_gas(&self, estimate: u64) -> u64 {
        let buffered = (estimate as u128).saturating_mul(100 + self.gas_buffer_pct) / 100;
        buffered.min(u64::MAX as u128) as u64
    }
}

/// The target's own fees, so `gas_oracle.bump` clears geth's +10% RBF floor. `None` from
/// `decode_fees` (non-1559 or undecodable) is terminal — nothing to bump against.
fn fee_basis(signed: &Bytes) -> Result<Eip1559Estimation, TransactionManagerError> {
    signing::decode_fees(signed)
        .map(|f| f.fees)
        .ok_or(TransactionManagerError::CancelTerminal)
}

/// Why a send/cancel/bump through the pipeline failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransactionManagerError {
    /// The signer's address does not control the intent's account.
    #[error("signer {signer} does not control the intent account {intent}")]
    AccountMismatch {
        /// The intent's declared account.
        intent: Address,
        /// The signer's actual address.
        signer: Address,
    },
    /// Pre-send simulation rejected the intent (it would revert).
    #[error("simulation rejected: {reason}")]
    SimulationRejected {
        /// The decoded revert/rejection reason.
        reason: String,
    },
    /// No tracked transaction matches the given handle id.
    #[error("no tracked transaction for this handle id")]
    UnknownHandle,
    /// The transaction already reached a terminal state — nothing to cancel.
    #[error("the transaction already settled — nothing to cancel")]
    CancelTerminal,
    /// Policy denied the intent.
    #[error(transparent)]
    Denied(PolicyRejection),
    /// An RPC call failed.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Fee estimation/bumping failed.
    #[error(transparent)]
    Gas(#[from] GasOracleError),
    /// Policy evaluation failed operationally.
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
    /// Broadcasting failed.
    #[error(transparent)]
    Submission(#[from] SubmissionError),
    /// The submission route was not valid for the chosen relay.
    #[error(transparent)]
    Route(#[from] RouteError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{
        CallLog, Harness, MockGas, MockNonce, MockPolicy, MockRpc, MockSigner, MockStore,
        MockSubmit, Submit, intent, shared_log,
    };
    use alloy_primitives::address;

    /// Wires the pipeline sharing one call log across the mocks so a test can assert
    /// the exact stage order.
    fn manager(
        allow: bool,
        estimate_ok: bool,
        sign_ok: bool,
        submit: Submit,
    ) -> (TransactionManager, CallLog) {
        let l = shared_log();
        let tm = Harness::default()
            .rpc(Arc::new(MockRpc {
                gas_reverts: !estimate_ok,
                log: l.clone(),
                ..Default::default()
            }))
            .gas(Arc::new(MockGas {
                log: l.clone(),
                ..Default::default()
            }))
            .policy(Arc::new(MockPolicy {
                allow,
                log: l.clone(),
                ..Default::default()
            }))
            .nonce(Arc::new(MockNonce {
                next: 7,
                log: l.clone(),
            }))
            .signer(Arc::new(MockSigner {
                ok: sign_ok,
                log: l.clone(),
                ..Default::default()
            }))
            .submit(Arc::new(MockSubmit {
                outcome: submit,
                log: l.clone(),
                ..Default::default()
            }))
            .store(Arc::new(MockStore::logged(l.clone())))
            .manager();
        (tm, l)
    }

    #[tokio::test]
    async fn happy_path_runs_stages_in_order_and_returns_a_handle() {
        let (tm, l) = manager(true, true, true, Submit::Ok);
        let handle = tm.send(&intent()).await.unwrap();
        assert_eq!(
            *l.lock(),
            [
                "estimate_gas",
                "fees",
                "policy",
                "allocate",
                "sign",
                "persist", // WAL before broadcast
                "submit",
                "persist", // status -> Sent after broadcast
            ]
        );
        assert_eq!(handle.status, TxStatus::Sent);
        assert_eq!(handle.nonce, 7);
        assert_eq!(handle.broadcasts.len(), 1);
    }

    #[tokio::test]
    async fn policy_deny_aborts_before_allocating_a_nonce() {
        let (tm, l) = manager(false, true, true, Submit::Ok);
        assert!(matches!(
            tm.send(&intent()).await,
            Err(TransactionManagerError::Denied(_))
        ));
        assert!(!l.lock().contains(&"allocate"));
    }

    #[tokio::test]
    async fn estimate_revert_aborts_before_signing() {
        let (tm, l) = manager(true, false, true, Submit::Ok);
        assert!(matches!(
            tm.send(&intent()).await,
            Err(TransactionManagerError::SimulationRejected { .. })
        ));
        let seen = l.lock();
        assert!(!seen.contains(&"sign") && !seen.contains(&"allocate"));
    }

    #[tokio::test]
    async fn send_rejects_when_signer_does_not_control_the_account() {
        let (tm, l) = manager(true, true, true, Submit::Ok); // signer address is ZERO
        let mut mismatched = intent();
        mismatched.account = address!("0x00000000000000000000000000000000000000bb");
        assert!(matches!(
            tm.send(&mismatched).await,
            Err(TransactionManagerError::AccountMismatch { .. })
        ));
        assert!(l.lock().is_empty()); // aborts before any RPC
    }

    #[tokio::test]
    async fn transient_submit_failure_assumes_sent_and_keeps_the_nonce() {
        // Indeterminate submit (may be in-flight) -> assume sent: Ok, Sent, nonce NOT released.
        let (tm, l) = manager(true, true, true, Submit::Transient);
        let handle = tm.send(&intent()).await.unwrap();
        assert_eq!(handle.status, TxStatus::Sent);
        assert!(!l.lock().contains(&"release"));
    }

    #[tokio::test]
    async fn deterministic_submit_reject_releases_the_nonce() {
        // Definitely not broadcast -> terminalize + recycle the nonce.
        let (tm, l) = manager(true, true, true, Submit::Deterministic);
        assert!(matches!(
            tm.send(&intent()).await,
            Err(TransactionManagerError::Submission(_))
        ));
        let seen = l.lock();
        assert!(seen.contains(&"allocate") && seen.contains(&"release"));
    }

    #[tokio::test]
    async fn already_known_submit_assumes_sent_and_keeps_the_nonce() {
        // "already known"/"nonce too low" == already accepted -> Sent, nonce NOT released.
        let (tm, l) = manager(true, true, true, Submit::AlreadyKnown);
        let handle = tm.send(&intent()).await.unwrap();
        assert_eq!(handle.status, TxStatus::Sent);
        assert!(!l.lock().contains(&"release"));
    }

    #[tokio::test]
    async fn sign_failure_recycles_the_nonce_so_the_next_send_reuses_it() {
        // A pre-broadcast sign failure must release the allocated nonce, so the next send
        // reuses it with no gap. Proven end-to-end against the real allocator + a shared
        // store (only the signer differs): without the release branch the retry would
        // allocate 6, not 5.
        use crate::adapters::{InMemoryStateStore, LocalNonceManager};
        let store = Arc::new(InMemoryStateStore::default());
        let rpc = Arc::new(MockRpc {
            pending_nonce: 5,
            ..Default::default()
        });
        let nonce = Arc::new(LocalNonceManager::new(store.clone(), rpc.clone()));
        let build = |sign_ok: bool| {
            Harness::default()
                .rpc(rpc.clone())
                .nonce(nonce.clone())
                .store(store.clone())
                .signer(Arc::new(MockSigner {
                    ok: sign_ok,
                    ..Default::default()
                }))
                .manager()
        };

        // First send: sign fails after allocating nonce 5 -> release it.
        let failed = build(false).send(&intent()).await;
        assert!(matches!(failed, Err(TransactionManagerError::Signer(_))));

        // Second send: the freed nonce 5 is reused (no gap), not 6.
        let recovered = build(true).send(&intent()).await.unwrap();
        assert_eq!(recovered.nonce, 5);
    }

    // --- cancel(id) ---

    #[tokio::test]
    async fn cancel_on_terminal_handle_errors() {
        use crate::testutils::handle;
        let store = Arc::new(MockStore::default());
        let done = handle(5, TxStatus::Confirmed { block: 12 });
        store.put_handle(&done).await.unwrap();
        let tm = Harness::default().store(store).manager();

        assert!(matches!(
            tm.cancel(done.id).await,
            Err(TransactionManagerError::CancelTerminal)
        ));
    }

    // --- gasless (J) ---

    #[tokio::test]
    async fn forwarder_nonce_rejects_a_reverting_address_as_not_a_forwarder() {
        // A `nonces()` that reverts (or returns junk) means the configured address isn't a
        // conforming forwarder — a terminal config error, not a transient read. (The happy
        // read + field mapping are covered by the facade self-relay test and the anvil parity
        // test against a real forwarder.)
        let tm = Harness::default()
            .rpc(Arc::new(MockRpc {
                call_reverts: true,
                ..Default::default()
            }))
            .manager();
        let err = tm
            .forwarder_nonce(Address::repeat_byte(0xfe), Address::ZERO)
            .await
            .unwrap_err();
        assert!(matches!(err, RelayError::Forwarder { .. }));
    }
}
