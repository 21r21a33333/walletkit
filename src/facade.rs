//! `Wallet` — the composition root: wires the eight adapters into one account's
//! runtime (the send pipeline + the tracking executor) behind a small public API.
//! One `Wallet` is one account (the signer defines it), so single-executor-per-account
//! is structural. Host-driven: [`tick`](Wallet::tick) runs one recover→confirm→escalate
//! pass; a background runner is opt-in sugar.

use crate::adapters::policy::{AllowAll, DefaultPolicyEngine};
use crate::adapters::{
    InMemoryStateStore, LocalNonceManager, PublicMempool, RpcGasOracle, SystemClock, Transport,
};
use crate::core::deps::{Clock, PolicyEngine, Rpc, Signer, StateStore};
use crate::core::wallet::{
    AccountExecutor, HandleId, PolicyOutcome, SignatureEnvelope, SigningRequest,
    TransactionManager, TxHandle, TxIntent, TxPreview, TxStatus, dry_run,
};
use crate::error::WalletKitError;
use alloy_dyn_abi::TypedData;
use alloy_primitives::Address;
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

    /// Cancel a pending tx: a policy-gated 0-value self-send at its nonce (RBF). Errors if
    /// the tx already settled. The original settles as `Dropped` once the cancel mines.
    pub async fn cancel(&self, id: HandleId) -> Result<TxHandle, WalletKitError> {
        Ok(self.manager.cancel(id).await?)
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

    /// One executor cycle: recover in-flight → confirm progress → escalate stuck.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "debug", skip_all))]
    pub async fn tick(&self) -> Result<(), WalletKitError> {
        Ok(self.executor.tick().await?)
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
}

impl WalletBuilder {
    pub fn confirmations(mut self, depth: u64) -> Self {
        self.confirmations = Some(depth);
        self
    }
    pub fn bump_timeout(mut self, secs: u64) -> Self {
        self.bump_timeout = Some(secs);
        self
    }
    pub fn gas_ceiling(mut self, wei: u128) -> Self {
        self.gas_ceiling = wei;
        self
    }
    pub fn gas_buffer_pct(mut self, pct: u128) -> Self {
        self.gas_buffer_pct = Some(pct);
        self
    }
    pub fn store(mut self, store: Arc<dyn StateStore>) -> Self {
        self.store = Some(store);
        self
    }
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
        let submission = Arc::new(PublicMempool::new(self.rpc.clone()));

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

        let mut executor = AccountExecutor::new(
            self.rpc.clone(),
            nonce_manager,
            submission,
            store.clone(),
            gas_oracle,
            self.policy.clone(),
            self.signer,
            clock,
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

        Wallet {
            manager,
            executor,
            store,
            rpc: self.rpc,
            policy: self.policy,
            account,
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
}
