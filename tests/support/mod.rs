//! Embedded-anvil integration harness, parameterized by storage [`Backend`] so the
//! localnet suite runs the same scenarios over in-memory, redb, and Postgres.
//! [`Localnet::spawn_on`] returns `None` — a clean skip — when anvil is absent or the
//! chosen backend is unavailable (no `DATABASE_URL` for Postgres). Every wallet tx targets
//! the allowlisted [`RECIPIENT`]; the native policy is default-deny and `PolicyApproval`
//! can't be minted outside the crate.

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, TxKind, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use std::sync::Arc;
use tempfile::TempDir;
use url::Url;
use walletkit::Wallet;
#[cfg(feature = "postgres")]
use walletkit::adapters::PostgresStateStore;
#[cfg(feature = "redb")]
use walletkit::adapters::RedbStateStore;
use walletkit::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};
use walletkit::adapters::{InMemoryStateStore, LocalSigner, SystemClock, Transport};
use walletkit::core::deps::{Clock, PolicyEngine, Signer, StateStore};
use walletkit::core::wallet::TxIntent;

/// Anvil's default dev mnemonic (Foundry default) — the first 10 accounts are funded.
const ANVIL_MNEMONIC: &str = "test test test test test test test test test test test junk";

/// The single allowlisted destination for every wallet tx (the recipient is irrelevant to
/// what these tests assert — nonce/confirm/reorg behavior).
pub const RECIPIENT: Address = Address::new([0xbb; 20]);

/// Which durable backend the store adapter uses. Variants track the compiled features, so a
/// `--no-default-features` build sees only [`Backend::InMemory`].
#[derive(Debug, Clone, Copy)]
pub enum Backend {
    InMemory,
    #[cfg(feature = "redb")]
    Redb,
    #[cfg(feature = "postgres")]
    Postgres,
}

pub struct Localnet {
    _anvil: AnvilInstance,
    pub wallet: Arc<Wallet>,
    /// Raw alloy provider for chain control (mining, reorg, external txs).
    pub control: DynProvider,
    pub account: Address,
    /// Shared across a restart so a rebuilt wallet picks up the persisted state.
    store: Arc<dyn StateStore>,
    /// Holds a redb backend's temp dir open for the harness lifetime.
    _store_tmp: Option<TempDir>,
    endpoint: Url,
    account_index: u32,
    confirmations: u64,
}

impl Localnet {
    /// Spawn a fresh anvil + a `Wallet` (signing account `account_index`) over `backend` at
    /// `confirmations` depth. `None` — a clean skip — when anvil or the backend is absent.
    pub async fn spawn_on(
        backend: Backend,
        account_index: u32,
        confirmations: u64,
    ) -> Option<Localnet> {
        // `--slots-in-an-epoch 1` advances anvil's `finalized` tag ~1 block (vs ~64), so the
        // executor's finalized-anchored confirm settles within a few mined blocks.
        let anvil = Anvil::new()
            .arg("--slots-in-an-epoch")
            .arg("1")
            .try_spawn()
            .ok()?;
        let endpoint = anvil.endpoint_url();
        let account = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, account_index)
            .ok()?
            .address();
        let (store, _store_tmp) = build_store(backend, account).await?;
        let wallet = build_wallet(&endpoint, account_index, store.clone(), confirmations)?;
        let control = ProviderBuilder::new()
            .connect_http(endpoint.clone())
            .erased();
        Some(Localnet {
            _anvil: anvil,
            wallet: Arc::new(wallet),
            control,
            account,
            store,
            _store_tmp,
            endpoint,
            account_index,
            confirmations,
        })
    }

    /// Rebuild a fresh `Wallet` over the **same** store — a restart picking up the persisted
    /// in-flight handles.
    pub fn rebuild_wallet(&self) -> Arc<Wallet> {
        Arc::new(
            build_wallet(
                &self.endpoint,
                self.account_index,
                self.store.clone(),
                self.confirmations,
            )
            .expect("rebuild wallet"),
        )
    }

    /// A value-transfer intent from this wallet's account to the allowlisted recipient.
    pub fn intent(&self, value: u64) -> TxIntent {
        self.intent_wei(U256::from(value))
    }

    /// As [`intent`](Self::intent) but with an arbitrary wei value (e.g. an over-balance
    /// amount to trip the estimate gate).
    pub fn intent_wei(&self, value: U256) -> TxIntent {
        TxIntent {
            chain_id: self.chain_id(),
            account: self.account,
            to: TxKind::Call(RECIPIENT),
            value,
            input: Default::default(),
            purpose: None,
        }
    }

    pub fn chain_id(&self) -> u64 {
        self._anvil.chain_id()
    }

    /// Mine `n` blocks via `anvil_mine`.
    pub async fn mine(&self, n: u64) {
        let _: () = self
            .control
            .raw_request("anvil_mine".into(), (n,))
            .await
            .expect("anvil_mine");
    }

    /// Reorg `depth` blocks: drop the most recent `depth` and rebuild without the dropped
    /// txs (they return to the pool).
    pub async fn reorg(&self, depth: u64) {
        let no_injected_txs: Vec<u64> = Vec::new();
        let _: () = self
            .control
            .raw_request("anvil_reorg".into(), (depth, no_injected_txs))
            .await
            .expect("anvil_reorg");
    }

    /// Stop auto-mining so submitted txs stay pending in the pool.
    pub async fn no_auto_mine(&self) {
        let _: () = self
            .control
            .raw_request("evm_setAutomine".into(), (false,))
            .await
            .expect("evm_setAutomine");
    }

    /// Send a same-key, out-of-band tx from the wallet's account at `nonce`. Its fee clears
    /// the RBF threshold over the wallet's pending tx (so it replaces it with mining off) but
    /// stays modest — a huge tip would skew anvil's fee history past the wallet's next
    /// approval envelope. The next mine settles it, consuming the nonce.
    pub async fn steal_nonce(&self, nonce: u64) {
        use alloy_rpc_types_eth::TransactionRequest;
        let tx = TransactionRequest {
            from: Some(self.account),
            to: Some(TxKind::Call(self.account)),
            value: Some(U256::from(1u64)),
            nonce: Some(nonce),
            max_fee_per_gas: Some(30_000_000_000), // 30 gwei
            max_priority_fee_per_gas: Some(5_000_000_000), // 5 gwei
            ..Default::default()
        };
        let _ = self
            .control
            .send_transaction(tx)
            .await
            .expect("foreign tx submit");
    }
}

/// Build the store for `backend`, resetting `account` for a deterministic run (redb gets a
/// fresh temp file; Postgres clears the account's rows). `None` skips an unavailable backend.
#[cfg_attr(not(feature = "postgres"), allow(unused_variables))] // `account` only clears Postgres rows
async fn build_store(
    backend: Backend,
    account: Address,
) -> Option<(Arc<dyn StateStore>, Option<TempDir>)> {
    match backend {
        Backend::InMemory => Some((Arc::new(InMemoryStateStore::default()), None)),
        #[cfg(feature = "redb")]
        Backend::Redb => {
            let tmp = tempfile::tempdir().ok()?;
            let store = RedbStateStore::open(tmp.path().join("wk.redb")).ok()?;
            Some((Arc::new(store), Some(tmp)))
        }
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let store = PostgresStateStore::connect(&std::env::var("DATABASE_URL").ok()?)
                .await
                .ok()?;
            store.clear_account(account).await.ok()?;
            Some((Arc::new(store), None))
        }
    }
}

/// Build a `Wallet` over `account_index`, an allow-`RECIPIENT` policy, and `store`.
fn build_wallet(
    endpoint: &Url,
    account_index: u32,
    store: Arc<dyn StateStore>,
    confirmations: u64,
) -> Option<Wallet> {
    let signer = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, account_index).ok()?;
    let transport = Transport::single(endpoint.clone()).ok()?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let policy: Arc<dyn PolicyEngine> = Arc::new(DefaultPolicyEngine::new(
        vec![Box::new(TargetAllowlist::new([RECIPIENT]))],
        clock,
    ));
    Some(
        Wallet::builder(Arc::new(transport), Arc::new(signer), policy)
            .confirmations(confirmations)
            .bump_timeout(0)
            .gas_ceiling(u128::MAX)
            .store(store)
            .build(),
    )
}
