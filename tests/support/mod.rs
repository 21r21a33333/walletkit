//! Embedded-anvil integration harness, parameterized by storage [`Backend`] so the
//! localnet suite runs the same scenarios over in-memory, redb, and Postgres.
//! [`Localnet::spawn_on`] returns `None` — a clean skip — when anvil is absent or the
//! chosen backend is unavailable (no `DATABASE_URL` for Postgres). Every wallet tx targets
//! the allowlisted [`RECIPIENT`]; the native policy is default-deny and `PolicyApproval`
//! can't be minted outside the crate.
//!
//! Shared across every `tests/*.rs` binary; each uses a different subset of helpers, so
//! per-binary `dead_code` is expected and silenced here.
#![allow(dead_code)]

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_sol_types::sol;
use std::sync::Arc;
use tempfile::TempDir;
use url::Url;
use walletkit::Wallet;

sol! {
    /// The read/preview integration fixture (see `mock_erc20.bin`). `revertWith` reverts
    /// with `Error("nope")` for the preview revert-decode test.
    #[sol(rpc)]
    interface MockErc20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function revertWith() external pure;
    }
}
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
        // executor's finalized-anchored confirm settles within a few mined blocks. `--accounts
        // 16` funds enough dev accounts for every scenario's distinct signing account.
        let anvil = Anvil::new()
            .arg("--slots-in-an-epoch")
            .arg("1")
            .arg("--accounts")
            .arg("16")
            .try_spawn()
            .ok()?;
        let endpoint = anvil.endpoint_url();
        let account = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, account_index)
            .ok()?
            .address();
        let (store, _store_tmp) = build_store(backend, account).await?;
        let wallet = build_wallet(
            &endpoint,
            account_index,
            store.clone(),
            confirmations,
            false,
        )?;
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
                false,
            )
            .expect("rebuild wallet"),
        )
    }

    /// A wallet with intent-refill enabled, over the same store — for the refill scenarios.
    pub fn refill_wallet(&self) -> Arc<Wallet> {
        Arc::new(
            build_wallet(
                &self.endpoint,
                self.account_index,
                self.store.clone(),
                self.confirmations,
                true,
            )
            .expect("refill wallet"),
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

    /// The node's HTTP endpoint (for building a standalone `Transport`/read client).
    pub fn endpoint(&self) -> Url {
        self.endpoint.clone()
    }

    /// Address of funded anvil dev account `index` (the harness spawns 16).
    pub fn account_at(&self, index: u32) -> Address {
        LocalSigner::from_mnemonic(ANVIL_MNEMONIC, index)
            .expect("mnemonic")
            .address()
    }

    /// Deploy the committed mock ERC-20 from funded account `deployer_index` and return its
    /// address. The constructor mints the full supply to the deployer.
    pub async fn deploy_mock_erc20(&self, deployer_index: u32) -> Address {
        use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
        let hex = include_str!("fixtures/mock_erc20.bin").trim();
        let code = Bytes::from(alloy_primitives::hex::decode(hex).expect("valid hex"));
        let tx = TransactionRequest {
            from: Some(self.account_at(deployer_index)),
            input: TransactionInput::new(code),
            ..Default::default()
        };
        let receipt = self
            .control
            .send_transaction(tx)
            .await
            .expect("deploy send")
            .get_receipt()
            .await
            .expect("deploy receipt");
        receipt.contract_address.expect("contract address")
    }

    /// Inject the canonical Multicall3 at its well-known address via `anvil_setCode`. anvil
    /// doesn't predeploy it, but real chains have it (keyless deploy), so reads that batch
    /// through it need it present.
    pub async fn deploy_multicall3(&self) {
        const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
        let code = include_str!("fixtures/multicall3.bin").trim();
        let _: () = self
            .control
            .raw_request("anvil_setCode".into(), (MULTICALL3, code))
            .await
            .expect("anvil_setCode multicall3");
    }

    /// Send a no-value contract call from funded account `from_index` and wait for it to mine.
    pub async fn send_tx(&self, from_index: u32, to: Address, input: Bytes) {
        use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
        let tx = TransactionRequest {
            from: Some(self.account_at(from_index)),
            to: Some(TxKind::Call(to)),
            input: TransactionInput::new(input),
            ..Default::default()
        };
        self.control
            .send_transaction(tx)
            .await
            .expect("send")
            .get_receipt()
            .await
            .expect("receipt");
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

/// A mainnet-forked anvil pinned at a fixed block: every read resolves at that immutable
/// state, so exact real-chain values can be asserted without rotting as balances change.
/// `None` — a clean skip — when anvil is unavailable or the (archive) upstream won't serve
/// the fork block.
pub struct ForkedNet {
    _anvil: AnvilInstance,
    endpoint: Url,
}

impl ForkedNet {
    /// Fork `rpc_url` (must serve archive state at `block`) and pin at `block`.
    pub async fn pin(rpc_url: &str, block: u64) -> Option<ForkedNet> {
        let anvil = Anvil::new()
            .arg("--fork-url")
            .arg(rpc_url)
            .arg("--fork-block-number")
            .arg(block.to_string())
            .try_spawn()
            .ok()?;
        let endpoint = anvil.endpoint_url();
        Some(ForkedNet {
            _anvil: anvil,
            endpoint,
        })
    }

    pub fn endpoint(&self) -> Url {
        self.endpoint.clone()
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
    refill_on_replaced: bool,
) -> Option<Wallet> {
    let signer = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, account_index).ok()?;
    let transport = Transport::url(endpoint.clone()).ok()?;
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
            .refill_on_replaced(refill_on_replaced)
            .store(store)
            .build(),
    )
}
