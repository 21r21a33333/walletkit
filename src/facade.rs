//! `Wallet` — the composition root: wires the eight adapters into one account's
//! runtime (the send pipeline + the tracking executor) behind a small public API.
//! One `Wallet` is one account (the signer defines it), so single-executor-per-account
//! is structural. Host-driven: [`tick`](Wallet::tick) runs one recover→confirm→escalate
//! pass; a background runner is opt-in sugar.

use crate::adapters::{
    InMemoryStateStore, LocalNonceManager, PublicMempool, RpcGasOracle, SystemClock,
};
use crate::core::deps::{Clock, PolicyEngine, Rpc, Signer, StateStore};
use crate::core::wallet::{
    AccountExecutor, HandleId, SignatureEnvelope, TransactionManager, TxHandle, TxIntent, TxStatus,
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
    manager: TransactionManager,
    executor: AccountExecutor,
    store: Arc<dyn StateStore>,
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
        }
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

        let mut executor = AccountExecutor::new(
            self.rpc,
            nonce_manager,
            submission,
            store.clone(),
            gas_oracle,
            self.policy,
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

        Wallet {
            manager,
            executor,
            store,
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
}
