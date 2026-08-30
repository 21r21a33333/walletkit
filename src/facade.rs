//! `Wallet` — the composition root: wires the eight adapters into one account's
//! runtime (the send pipeline + the tracking executor) behind a small public API.
//! One `Wallet` is one account (the signer defines it), so single-executor-per-account
//! is structural. Host-driven: [`tick`](Wallet::tick) runs one recover→confirm→escalate
//! pass; a background runner is opt-in sugar.

use crate::adapters::policy::{AllowAll, DefaultPolicyEngine};
use crate::adapters::{
    GelatoRelay, InMemoryStateStore, LocalNonceManager, PrivateMev, PublicMempool, Router,
    RpcGasOracle, SystemClock, Transport,
};
use crate::core::deps::{
    Clock, Deadline, GaslessOpts, GaslessRoute, Gelato, PolicyEngine, Relay, RelayError, Rpc,
    Signer, StateStore, SubmissionOpts, SubmissionStrategy,
};
use crate::core::wallet::{
    AccountExecutor, ForwarderDomain, HandleId, MetaContext, PolicyOutcome, SignatureEnvelope,
    SigningRequest, TransactionManager, TxHandle, TxIntent, TxPreview, TxStatus, dry_run,
    execute_calldata,
};
use crate::error::WalletKitError;
use alloy_dyn_abi::TypedData;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// A per-account wallet runtime. Build it with [`Wallet::builder`], `send` intents,
/// query `status`, and drive `tick` (or `run`) to confirm and bump.
pub struct Wallet {
    manager: Arc<TransactionManager>,
    executor: AccountExecutor,
    store: Arc<dyn StateStore>,
    rpc: Arc<dyn Rpc>,
    policy: Arc<dyn PolicyEngine>,
    account: Address,
    /// The wallet's gasless backend, if one is configured — at most one (self-relay *or* Gelato).
    gasless: Option<GaslessBackend>,
}

/// A wallet's single gasless backend. The two are mutually exclusive by construction — a wallet
/// relays gaslessly one way, so an enum (not two independent `Option`s) is the honest model.
enum GaslessBackend {
    /// Self-relay (Model 1): a second operated account submits + tracks the outer `execute()` tx.
    /// Boxed — a [`GaslessRuntime`] (a whole second executor) dwarfs the one-word `Gelato` variant.
    SelfRelay(Box<GaslessRuntime>),
    /// Managed relay (Gelato): a third party submits + pays; the user's executor polls its task.
    Gelato(Arc<GelatoRelay>),
}

/// The relayer account's runtime for self-relay gasless meta-transactions (Model 1): a second
/// operated account with its own send pipeline + tracking executor, distinct from the user account.
struct GaslessRuntime {
    /// The relayer's send pipeline (signs and pays for the outer `execute()` tx).
    manager: Arc<TransactionManager>,
    /// The relayer's tracking executor — confirms/bumps the outer tx and honors `handle.meta`.
    executor: AccountExecutor,
    /// The `ERC2771Forwarder` the user's requests are bound to and relayed through.
    forwarder: Address,
}

impl Wallet {
    /// Start a builder over the three ports a caller must choose (chain access,
    /// signing key, policy); the gas/nonce/submission/store/clock adapters are wired
    /// with sensible defaults and overridable knobs.
    pub fn builder(
        rpc: Arc<dyn Rpc>,
        signer: Arc<dyn Signer>,
        policy: Arc<dyn PolicyEngine>,
    ) -> WalletBuilder {
        WalletBuilder {
            rpc,
            signer,
            policy,
            store: None,
            clock: None,
            confirmations: None,
            bump_timeout: None,
            gas_ceiling: u128::MAX,
            gas_buffer_pct: None,
            refill_on_replaced: false,
            relay_identity: None,
            relayer: None,
            forwarder: None,
            relayer_policy: None,
            gelato: None,
        }
    }

    /// The common case in one call: build the HTTP transport from `url`, wrap `signer` and
    /// `policy`, and apply the default tracking config. Fallible only where inputs can be bad
    /// — a malformed URL or a transport that won't build. The policy stays explicit; the
    /// guardrail is never defaulted away.
    pub fn connect_http(
        url: &str,
        signer: impl Signer + 'static,
        policy: impl PolicyEngine + 'static,
    ) -> Result<Wallet, WalletKitError> {
        let parsed = url
            .parse::<url::Url>()
            .map_err(|e| WalletKitError::Connect(e.to_string()))?;
        let transport =
            Transport::url(parsed).map_err(|e| WalletKitError::Connect(e.to_string()))?;
        Ok(Wallet::builder(Arc::new(transport), Arc::new(signer), Arc::new(policy)).build())
    }

    /// **DEV/TEST ONLY** — like [`connect_http`](Self::connect_http) but with an allow-all
    /// policy, so every intent is permitted. Named loudly so shipping it to production is a
    /// deliberate choice, never an accidental default. Use [`connect_http`](Self::connect_http)
    /// with a real [`PolicyEngine`] anywhere the guardrail matters.
    pub fn connect_http_dev(
        url: &str,
        signer: impl Signer + 'static,
    ) -> Result<Wallet, WalletKitError> {
        let policy = DefaultPolicyEngine::new(vec![Box::new(AllowAll)], Arc::new(SystemClock));
        Self::connect_http(url, signer, policy)
    }

    /// The account this wallet signs for.
    pub fn account(&self) -> Address {
        self.account
    }

    /// Build, sign, and submit an intent, returning its tracked handle. Tracking,
    /// bumping, and confirmation happen on later [`tick`](Wallet::tick)s.
    pub async fn send(&self, intent: &TxIntent) -> Result<TxHandle, WalletKitError> {
        Ok(self.manager.send(intent).await?)
    }

    /// Like [`send`](Self::send) but choosing the broadcast route via `opts` — pass a
    /// [`Flashbots`](crate::core::deps::Flashbots) or [`Protect`](crate::core::deps::Protect)
    /// route directly. A private route requires a relay identity
    /// ([`WalletBuilder::relay_identity`]); without one the send fails cleanly (before signing)
    /// rather than leaking to the public mempool.
    pub async fn send_with(
        &self,
        intent: &TxIntent,
        opts: impl Into<SubmissionOpts>,
    ) -> Result<TxHandle, WalletKitError> {
        Ok(self.manager.send_with(intent, &opts.into()).await?)
    }

    /// Cancel a pending tx: a policy-gated 0-value self-send at its nonce (RBF). Errors if
    /// the tx already settled. The original settles as `Dropped` once the cancel mines.
    pub async fn cancel(&self, id: HandleId) -> Result<TxHandle, WalletKitError> {
        Ok(self.manager.cancel(id).await?)
    }

    /// Send `intent` gaslessly (ERC-2771): the user signs a free request (no ETH) and a third party
    /// submits + pays, while the target still sees the *user* as `_msgSender`. `opts` selects the
    /// backend — [`SelfRelay`](crate::core::deps::SelfRelay) (our relayer account pays) or
    /// [`GaslessOpts::gelato`] (a managed Gelato relay pays). Returns the tracked handle. If the
    /// selected backend isn't configured ([`WalletBuilder::relayer`]/[`forwarder`](WalletBuilder::forwarder)
    /// for self-relay, [`gelato`](WalletBuilder::gelato) for Gelato), it fails cleanly **before**
    /// signing — never a panic, never a leak to the user's account.
    pub async fn send_gasless(
        &self,
        intent: &TxIntent,
        opts: impl Into<GaslessOpts>,
    ) -> Result<TxHandle, WalletKitError> {
        let opts = opts.into();
        // Dispatch to the configured backend; a route with no matching backend (or none at all)
        // is the one clean rejection point — terminal, before any signing.
        match (opts.route, &self.gasless) {
            (GaslessRoute::SelfRelay(route), Some(GaslessBackend::SelfRelay(runtime))) => {
                self.send_self_relay(runtime, intent, route.submission, opts.deadline)
                    .await
            }
            (GaslessRoute::Gelato, Some(GaslessBackend::Gelato(relay))) => {
                self.send_gelato(relay, intent, opts.deadline).await
            }
            _ => Err(WalletKitError::Relay(RelayError::NotConfigured)),
        }
    }

    /// Self-relay (Model 1): our funded relayer account submits + pays for the outer `execute()`
    /// and its executor tracks it.
    async fn send_self_relay(
        &self,
        runtime: &GaslessRuntime,
        intent: &TxIntent,
        submission: SubmissionOpts,
        deadline: Deadline,
    ) -> Result<TxHandle, WalletKitError> {
        // The user authorizes + signs the request through *their* policy gate — never sends.
        let signed = self
            .manager
            .build_and_sign_forward_request(
                intent,
                runtime.forwarder,
                &ForwarderDomain::default(),
                deadline.0,
            )
            .await?;
        // The relayer submits and pays for the outer execute(), tracked under its account + meta.
        let outer = TxIntent {
            chain_id: intent.chain_id,
            account: runtime.manager.account(),
            to: TxKind::Call(runtime.forwarder),
            value: intent.value,
            input: execute_calldata(
                &signed.request,
                Bytes::from(signed.signature.as_bytes().to_vec()),
            ),
            purpose: None,
        };
        let meta = MetaContext::for_request(&signed);
        Ok(runtime
            .manager
            .send_with_meta(&outer, &submission, Some(meta))
            .await?)
    }

    /// Managed relay (Gelato): the user signs Gelato's own EIP-712 request through *their* policy
    /// gate, Gelato submits + pays, and the returned task is persisted under the user account for
    /// the executor to poll to inclusion.
    async fn send_gelato(
        &self,
        relay: &GelatoRelay,
        intent: &TxIntent,
        deadline: Deadline,
    ) -> Result<TxHandle, WalletKitError> {
        let deadline = U256::from(self.manager.now_unix().saturating_add(deadline.0.as_secs()));
        // The adapter reads the sequential nonce (or derives the concurrent salt) and shapes the
        // Gelato struct; the user signs it via the existing gate — gasless is not a policy bypass.
        let call = relay
            .build_call(intent, self.rpc.as_ref(), deadline)
            .await?;
        let signature = self
            .manager
            .sign_typed_data(&relay.typed_data(&call))
            .await?;
        let task = relay.submit(&call, &signature).await?;
        let meta = MetaContext::for_gelato_task(
            relay.verifying_contract(),
            call.user(),
            call.nonce(),
            task,
        );
        Ok(self.manager.persist_task_handle(intent, meta).await?)
    }

    /// Simulate an intent without signing or broadcasting: gas (advisory), success or a
    /// decoded revert reason, access list, and return data. A would-revert tx yields a
    /// [`TxPreview`] with a `Revert` outcome — not an error. Never touches the store.
    pub async fn dry_run(&self, intent: &TxIntent) -> Result<TxPreview, WalletKitError> {
        dry_run(self.rpc.as_ref(), intent)
            .await
            .map_err(WalletKitError::Rpc)
    }

    /// Dry-run this intent against the policy engine: **would** it be allowed, and if not,
    /// why? The policy analog of [`dry_run`](Self::dry_run) — no signing, no broadcast, and
    /// the returned [`PolicyOutcome`] cannot carry a signing capability. Advisory: the real
    /// gate re-runs at send time, so a passing validate is never a guarantee.
    pub async fn validate(&self, intent: &TxIntent) -> Result<PolicyOutcome, WalletKitError> {
        self.policy
            .validate(&SigningRequest::Transaction(intent.clone()))
            .await
            .map_err(WalletKitError::PolicyEngine)
    }

    /// Sign an EIP-191 `personal_sign` message (policy-gated; default-denied unless a rule
    /// allows message signing).
    pub async fn sign_message(&self, message: &[u8]) -> Result<SignatureEnvelope, WalletKitError> {
        Ok(self.manager.sign_message(message).await?)
    }

    /// Sign EIP-712 typed data (policy-gated; the domain `chainId` must be present and match
    /// a signable chain, and the `verifyingContract` must be allowlisted).
    pub async fn sign_typed_data(
        &self,
        typed: &TypedData,
    ) -> Result<SignatureEnvelope, WalletKitError> {
        Ok(self.manager.sign_typed_data(typed).await?)
    }

    /// The full tracked handle by id (terminal-inclusive), or `None` if the id is
    /// unknown — the queryable record (status, nonce, broadcasts, …).
    pub async fn handle(&self, id: HandleId) -> Result<Option<TxHandle>, WalletKitError> {
        Ok(self.store.handle(id).await?)
    }

    /// The current status of a tracked handle (terminal-inclusive), or `None` if the
    /// id is unknown.
    pub async fn status(&self, id: HandleId) -> Result<Option<TxStatus>, WalletKitError> {
        Ok(self.handle(id).await?.map(|handle| handle.status))
    }

    /// One executor cycle: recover in-flight → confirm progress → escalate stuck. Self-relay adds a
    /// *second* executor (the relayer account) driven on the same cadence; Gelato needs none — the
    /// user's own executor polls its task in the same confirm pass.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
    pub async fn tick(&self) -> Result<(), WalletKitError> {
        self.executor.tick().await?;
        if let Some(GaslessBackend::SelfRelay(runtime)) = &self.gasless {
            runtime.executor.tick().await?;
        }
        Ok(())
    }

    /// Spawn a background loop that ticks every `interval` — opt-in sugar over
    /// [`tick`](Wallet::tick) for hosts that don't run their own scheduler. A transient
    /// tick error is swallowed so one bad read can't kill the loop; call
    /// [`Runner::stop`] to end it.
    pub fn run(self: Arc<Self>, interval: Duration) -> Runner {
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let _ = self.tick().await;
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = &mut stop_rx => break,
                }
            }
        });
        Runner {
            stop: Some(stop_tx),
            task,
        }
    }
}

/// A running background tick loop returned by [`Wallet::run`]. Hold it to keep the
/// loop alive; call [`stop`](Runner::stop) to end it gracefully. Dropping it aborts
/// the task, but `stop` is the documented path — a silently-dropped loop is the exact
/// footgun ethers-rs's escalator hit (hence `#[must_use]`).
#[must_use = "the loop stops when the Runner is dropped — hold it and call stop()"]
pub struct Runner {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Runner {
    /// Signal the loop to finish its current pass and exit, then join it.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.task).await;
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Builder for [`Wallet`]. `gas_ceiling` defaults to `u128::MAX` (no global bump
/// ceiling — the per-intent approval envelope still caps spend); set it for a hard
/// per-tx fee cap. `store`/`clock` override the in-memory / system defaults.
pub struct WalletBuilder {
    rpc: Arc<dyn Rpc>,
    signer: Arc<dyn Signer>,
    policy: Arc<dyn PolicyEngine>,
    store: Option<Arc<dyn StateStore>>,
    clock: Option<Arc<dyn Clock>>,
    confirmations: Option<u64>,
    bump_timeout: Option<u64>,
    gas_ceiling: u128,
    gas_buffer_pct: Option<u128>,
    refill_on_replaced: bool,
    relay_identity: Option<PrivateKeySigner>,
    relayer: Option<Arc<dyn Signer>>,
    forwarder: Option<Address>,
    relayer_policy: Option<Arc<dyn PolicyEngine>>,
    gelato: Option<Gelato>,
}

impl WalletBuilder {
    /// Confirmation depth an outcome must reach before it is treated as terminal.
    pub fn confirmations(mut self, depth: u64) -> Self {
        self.confirmations = Some(depth);
        self
    }
    /// Seconds a pending tx may sit before the executor bumps its fees (RBF).
    pub fn bump_timeout(mut self, secs: u64) -> Self {
        self.bump_timeout = Some(secs);
        self
    }
    /// Absolute per-tx max-fee ceiling (wei); bumps stop rather than exceed it.
    pub fn gas_ceiling(mut self, wei: u128) -> Self {
        self.gas_ceiling = wei;
        self
    }
    /// Percentage padding added to the estimated gas limit.
    pub fn gas_buffer_pct(mut self, pct: u128) -> Self {
        self.gas_buffer_pct = Some(pct);
        self
    }
    /// Use a durable [`StateStore`] instead of the in-memory default (enables crash recovery).
    pub fn store(mut self, store: Arc<dyn StateStore>) -> Self {
        self.store = Some(store);
        self
    }
    /// Override the time source (e.g. for deterministic tests).
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }
    /// Re-execute an intent displaced by a *foreign* tx at a fresh nonce, retrying until it
    /// mines. Off by default; a cancelled tx is never refilled.
    pub fn refill_on_replaced(mut self, on: bool) -> Self {
        self.refill_on_replaced = on;
        self
    }

    /// Enable private-relay routing by supplying the endpoint-auth identity (the rotatable
    /// key that signs the `X-Flashbots-Signature` header — distinct from the tx-signing key).
    /// Without it, private sends are rejected. Set it, then choose a route per send via
    /// [`Wallet::send_with`].
    pub fn relay_identity(mut self, identity: PrivateKeySigner) -> Self {
        self.relay_identity = Some(identity);
        self
    }

    /// Enable gasless meta-transactions ([`Wallet::send_gasless`]) by supplying the **relayer**
    /// account — a funded signer that submits and pays for the outer `execute()` tx (distinct
    /// from the user signer, who only signs the free `ForwardRequest`). Also set
    /// [`forwarder`](Self::forwarder); without both, `send_gasless` fails cleanly. The relayer
    /// runs as its own operated account (its own tracking executor).
    pub fn relayer(mut self, relayer: Arc<dyn Signer>) -> Self {
        self.relayer = Some(relayer);
        self
    }

    /// The `ERC2771Forwarder` address gasless requests are bound to and relayed through. Pairs
    /// with [`relayer`](Self::relayer).
    pub fn forwarder(mut self, forwarder: Address) -> Self {
        self.forwarder = Some(forwarder);
        self
    }

    /// Policy for the **relayer's** outer `execute()` tx. Defaults to permissive
    /// ([`AllowAll`]) — the user's request was already gated on its own signing path, and the
    /// relayer's spend is infrastructure, not a user action. Override to cap the relayer (e.g. a
    /// per-tx value ceiling).
    pub fn relayer_policy(mut self, policy: Arc<dyn PolicyEngine>) -> Self {
        self.relayer_policy = Some(policy);
        self
    }

    /// Register a managed [`Gelato`] relay for `send_gasless(_, GaslessOpts::gelato())`. Gelato
    /// submits and pays; the sponsor key and fee/nonce scheme are set **once here** and never
    /// travel on a send call. Needs no relayer account or forwarder — Gelato *is* the operated
    /// relayer, and its public status endpoint is polled by the user's own executor.
    pub fn gelato(mut self, gelato: Gelato) -> Self {
        self.gelato = Some(gelato);
        self
    }

    /// Wire the runtime. Infallible — the account is `signer.address()` and every port
    /// is supplied, so there is nothing to validate.
    pub fn build(self) -> Wallet {
        let account = self.signer.address();
        let store: Arc<dyn StateStore> = self
            .store
            .unwrap_or_else(|| Arc::new(InMemoryStateStore::default()));
        let clock: Arc<dyn Clock> = self.clock.unwrap_or_else(|| Arc::new(SystemClock));
        let gas_oracle = Arc::new(RpcGasOracle::new(self.rpc.clone(), self.gas_ceiling));
        let nonce_manager = Arc::new(LocalNonceManager::new(store.clone(), self.rpc.clone()));
        // Always a Router — its private arm is present only with a relay identity, and the
        // Router rejects private sends without one, so the wallet holds no capability flag.
        let public = Arc::new(PublicMempool::new(self.rpc.clone()));
        let private: Option<Arc<dyn SubmissionStrategy>> = self.relay_identity.map(|id| {
            Arc::new(PrivateMev::new(self.rpc.clone(), id)) as Arc<dyn SubmissionStrategy>
        });
        let submission: Arc<dyn SubmissionStrategy> = Arc::new(Router::new(public, private));

        let mut manager = TransactionManager::new(
            self.rpc.clone(),
            gas_oracle.clone(),
            self.policy.clone(),
            nonce_manager.clone(),
            self.signer.clone(),
            submission.clone(),
            store.clone(),
            clock.clone(),
        );
        if let Some(pct) = self.gas_buffer_pct {
            manager = manager.with_gas_buffer_pct(pct);
        }
        let manager = Arc::new(manager);

        // Clone the shared adapters (a second, relayer executor may reuse them below); only the
        // user signer is consumed here.
        let mut executor = AccountExecutor::new(
            self.rpc.clone(),
            nonce_manager.clone(),
            submission.clone(),
            store.clone(),
            gas_oracle.clone(),
            self.policy.clone(),
            self.signer,
            clock.clone(),
            account,
        );
        if let Some(depth) = self.confirmations {
            executor = executor.with_confirmations(depth);
        }
        if let Some(secs) = self.bump_timeout {
            executor = executor.with_bump_timeout(secs);
        }
        if self.refill_on_replaced {
            executor = executor.with_refill(manager.clone());
        }

        // A wallet has at most one gasless backend. Self-relay (Model 1) configures a *second*
        // operated account — its own manager + executor — that sends and tracks the outer
        // `execute()` tx; its policy defaults to permissive (the user's request is already gated on
        // its own path). Managed Gelato needs no second account — Gelato submits and pays, so the
        // user's own executor polls its task. Configuring both is a wiring error; self-relay wins.
        debug_assert!(
            !(self.relayer.is_some() && self.forwarder.is_some() && self.gelato.is_some()),
            "configure at most one gasless backend (self-relay or Gelato), not both"
        );
        let gasless = match (self.relayer, self.forwarder, self.gelato) {
            (Some(relayer_signer), Some(forwarder), _) => {
                let relayer_account = relayer_signer.address();
                let relayer_policy = self.relayer_policy.unwrap_or_else(|| {
                    Arc::new(DefaultPolicyEngine::new(
                        vec![Box::new(AllowAll)],
                        clock.clone(),
                    )) as Arc<dyn PolicyEngine>
                });
                let relayer_manager = Arc::new(TransactionManager::new(
                    self.rpc.clone(),
                    gas_oracle.clone(),
                    relayer_policy.clone(),
                    nonce_manager.clone(),
                    relayer_signer.clone(),
                    submission.clone(),
                    store.clone(),
                    clock.clone(),
                ));
                let mut relayer_executor = AccountExecutor::new(
                    self.rpc.clone(),
                    nonce_manager,
                    submission,
                    store.clone(),
                    gas_oracle,
                    relayer_policy,
                    relayer_signer,
                    clock,
                    relayer_account,
                );
                if let Some(depth) = self.confirmations {
                    relayer_executor = relayer_executor.with_confirmations(depth);
                }
                if let Some(secs) = self.bump_timeout {
                    relayer_executor = relayer_executor.with_bump_timeout(secs);
                }
                Some(GaslessBackend::SelfRelay(Box::new(GaslessRuntime {
                    manager: relayer_manager,
                    executor: relayer_executor,
                    forwarder,
                })))
            }
            (_, _, Some(config)) => {
                let relay = Arc::new(GelatoRelay::new(config));
                executor = executor.with_relay(relay.clone() as Arc<dyn Relay>);
                Some(GaslessBackend::Gelato(relay))
            }
            _ => None,
        };

        Wallet {
            manager,
            executor,
            store,
            rpc: self.rpc,
            policy: self.policy,
            account,
            gasless,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils::{MockPolicy, MockRpc, MockSigner};

    #[tokio::test]
    async fn run_then_stop_terminates_promptly() {
        // The background loop must be stoppable — a dropped/leaked loop is the ethers-rs
        // footgun; stop() signals and joins it, and must not hang.
        let wallet = Arc::new(
            Wallet::builder(
                Arc::new(MockRpc::default()),
                Arc::new(MockSigner::default()),
                Arc::new(MockPolicy::default()),
            )
            .bump_timeout(0)
            .build(),
        );
        let running = wallet.run(Duration::from_millis(5));
        tokio::time::timeout(Duration::from_secs(2), running.stop())
            .await
            .expect("loop did not stop within 2s");
    }

    #[tokio::test]
    async fn private_send_without_relay_identity_is_rejected() {
        // With no relay identity, a `Private` route must fail cleanly before signing —
        // never fall through to the public mempool (a privacy leak in a release build).
        use crate::core::deps::{Escalation, Protect, RouteError};
        let wallet = Wallet::builder(
            Arc::new(MockRpc::default()),
            Arc::new(MockSigner::default()),
            Arc::new(MockPolicy::default()),
        )
        .build();
        let intent = TxIntent::transfer(
            1,
            Address::ZERO,
            Address::ZERO,
            alloy_primitives::U256::ZERO,
        );
        let err = wallet
            .send_with(&intent, Protect::mev_blocker(Escalation::StayPrivate))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            WalletKitError::Route(RouteError::RelayNotConfigured)
        ));
    }

    #[tokio::test]
    async fn sign_message_is_policy_gated_end_to_end() {
        use crate::adapters::LocalSigner;
        use crate::adapters::SystemClock;
        use crate::adapters::policy::{
            DefaultPolicyEngine, MessageSigningAllowed, TargetAllowlist,
        };

        const MNEMONIC: &str = "test test test test test test test test test test test junk";
        let signer = Arc::new(LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap());
        let account = signer.address();
        let msg = b"siwe login nonce";

        // A message rule grants -> the envelope recovers to the wallet's account.
        let allow = Arc::new(DefaultPolicyEngine::new(
            vec![Box::new(MessageSigningAllowed)],
            Arc::new(SystemClock),
        ));
        let wallet = Wallet::builder(Arc::new(MockRpc::default()), signer.clone(), allow).build();
        let env = wallet.sign_message(msg).await.expect("sign");
        assert_eq!(
            env.signature().recover_address_from_msg(msg).unwrap(),
            account
        );

        // No message rule -> default-deny surfaces as WalletKitError::Policy.
        let deny = Arc::new(DefaultPolicyEngine::new(
            vec![Box::new(TargetAllowlist::new([]))],
            Arc::new(SystemClock),
        ));
        let gated = Wallet::builder(Arc::new(MockRpc::default()), signer, deny).build();
        assert!(matches!(
            gated.sign_message(msg).await,
            Err(WalletKitError::Policy(_))
        ));
    }

    #[tokio::test]
    async fn validate_previews_the_policy_decision_without_signing() {
        use crate::adapters::SystemClock;
        use crate::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};
        use alloy_primitives::U256;

        let allowed = Address::from([0x22; 20]);
        let policy = Arc::new(DefaultPolicyEngine::new(
            vec![Box::new(TargetAllowlist::new([allowed]))],
            Arc::new(SystemClock),
        ));
        let wallet = Wallet::builder(
            Arc::new(MockRpc::default()),
            Arc::new(MockSigner::default()),
            policy,
        )
        .build();
        let account = wallet.account();

        let ok = TxIntent::transfer(1, account, allowed, U256::from(1u64));
        assert_eq!(
            wallet.validate(&ok).await.unwrap(),
            PolicyOutcome::WouldAllow
        );

        let bad = TxIntent::transfer(1, account, Address::from([0x99; 20]), U256::from(1u64));
        assert!(matches!(
            wallet.validate(&bad).await.unwrap(),
            PolicyOutcome::WouldDeny(_)
        ));
    }

    #[tokio::test]
    async fn send_gasless_without_a_relayer_is_rejected_before_signing() {
        // A wallet with no relayer/forwarder must reject a gasless send cleanly (terminal,
        // never a panic) before any signing — the guard the whole feature hangs off.
        use crate::core::deps::SelfRelay;
        use alloy_primitives::U256;
        let wallet = Wallet::builder(
            Arc::new(MockRpc::default()),
            Arc::new(MockSigner::default()),
            Arc::new(MockPolicy::default()),
        )
        .build();
        let intent = TxIntent::call(
            1,
            Address::ZERO,
            Address::from([0x22; 20]),
            U256::ZERO,
            alloy_primitives::Bytes::from_static(&[0xaa]),
        );
        let err = wallet
            .send_gasless(&intent, SelfRelay::new())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            WalletKitError::Relay(RelayError::NotConfigured)
        ));
    }

    #[tokio::test]
    async fn send_gasless_gelato_without_registration_is_rejected() {
        // Selecting the managed relay with no `.gelato(...)` registered must fail cleanly
        // (terminal), never a panic and never a leak onto the user's account.
        use crate::core::deps::GaslessOpts;
        use alloy_primitives::U256;
        let wallet = Wallet::builder(
            Arc::new(MockRpc::default()),
            Arc::new(MockSigner::default()),
            Arc::new(MockPolicy::default()),
        )
        .build();
        let intent = TxIntent::call(
            1,
            Address::ZERO,
            Address::from([0x22; 20]),
            U256::ZERO,
            alloy_primitives::Bytes::from_static(&[0xaa]),
        );
        let err = wallet
            .send_gasless(&intent, GaslessOpts::gelato())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            WalletKitError::Relay(RelayError::NotConfigured)
        ));
    }

    #[tokio::test]
    async fn send_gasless_self_relay_yields_a_relayer_tracked_meta_handle() {
        // The whole Model-1 path: the user signs the request (user policy + signer), the relayer
        // account sends the outer execute(), and the returned handle is the *relayer's*, stamped
        // with meta pointing at the forwarder + the user as signer.
        use crate::core::deps::SelfRelay;
        use crate::testutils::u256_word;
        use alloy_primitives::{Bytes, U256};

        let user = Address::from([0x11; 20]);
        let relayer = Address::from([0x22; 20]);
        let forwarder = Address::from([0xfe; 20]);
        let wallet = Wallet::builder(
            Arc::new(MockRpc {
                call_returns: Some(u256_word(0)),
                ..Default::default()
            }),
            Arc::new(MockSigner {
                address: user,
                ..Default::default()
            }),
            Arc::new(MockPolicy::default()),
        )
        .relayer(Arc::new(MockSigner {
            address: relayer,
            ..Default::default()
        }))
        .forwarder(forwarder)
        .build();

        let intent = TxIntent::call(
            1,
            user,
            Address::from([0x33; 20]),
            U256::ZERO,
            Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]),
        );
        let handle = wallet
            .send_gasless(&intent, SelfRelay::new())
            .await
            .expect("gasless send");

        assert_eq!(handle.account, relayer, "outer tx is the relayer's");
        let meta = handle.meta.expect("meta stamped for confirm-safety");
        assert_eq!(meta.forwarder, forwarder);
        assert_eq!(
            meta.signer, user,
            "the event we decode is the user's request"
        );
    }
}
