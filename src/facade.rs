//! `Wallet` — the composition root: wires the eight adapters into one account's
//! runtime (the send pipeline + the tracking executor) behind a small public API.
//! One `Wallet` is one account (the signer defines it), so single-executor-per-account
//! is structural. Host-driven: [`tick`](Wallet::tick) runs one recover→confirm→escalate
//! pass; a background runner is opt-in sugar (added next).

use crate::adapters::{
    InMemoryStateStore, LocalNonceManager, PublicMempool, RpcGasOracle, SystemClock,
};
use crate::core::deps::{Clock, PolicyEngine, Rpc, Signer, StateStore, StateStoreError};
use crate::core::wallet::{
    AccountExecutor, ExecutorError, HandleId, TransactionManager, TransactionManagerError,
    TxHandle, TxIntent, TxStatus,
};
use alloy_primitives::Address;
use std::sync::Arc;

/// A per-account wallet runtime. Build it with [`Wallet::builder`], `send` intents,
/// query `status`, and drive `tick` (or `run`) to confirm and bump.
pub struct Wallet {
    pipeline: TransactionManager,
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
    pub async fn send(&self, intent: &TxIntent) -> Result<TxHandle, WalletError> {
        Ok(self.pipeline.send(intent).await?)
    }

    /// The current status of a tracked handle (terminal-inclusive), or `None` if the
    /// id is unknown.
    pub async fn status(&self, id: HandleId) -> Result<Option<TxStatus>, WalletError> {
        Ok(self.store.handle(id).await?.map(|handle| handle.status))
    }

    /// One executor cycle: recover in-flight → confirm progress → escalate stuck.
    pub async fn tick(&self) -> Result<(), WalletError> {
        Ok(self.executor.tick().await?)
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

        let mut pipeline = TransactionManager::new(
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
            pipeline = pipeline.with_gas_buffer_pct(pct);
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
            pipeline,
            executor,
            store,
            account,
        }
    }
}

/// Operational failures the wallet surfaces from its send pipeline, tracking
/// executor, or state store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WalletError {
    #[error(transparent)]
    Send(#[from] TransactionManagerError),
    #[error(transparent)]
    Execute(#[from] ExecutorError),
    #[error(transparent)]
    Store(#[from] StateStoreError),
}
