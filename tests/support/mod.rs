//! Embedded-anvil integration harness. [`Localnet::spawn`] returns `None` when the
//! `anvil` binary isn't on PATH, so the localnet suite is a clean no-op without
//! Foundry. Every wallet tx goes to one allowlisted [`RECIPIENT`] because the native
//! policy is default-deny and `PolicyApproval` can't be minted outside the crate.

use alloy_node_bindings::{Anvil, AnvilInstance};
use alloy_primitives::{Address, TxKind, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use std::sync::Arc;
use walletkit::Wallet;
use walletkit::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};
use walletkit::adapters::{LocalSigner, SystemClock, Transport};
use walletkit::core::deps::{Clock, PolicyEngine, Signer};
use walletkit::core::wallet::TxIntent;

/// Anvil's default dev mnemonic (Foundry default) — account 0 is funded.
const ANVIL_MNEMONIC: &str = "test test test test test test test test test test test junk";

/// The single allowlisted destination for every wallet tx (the recipient is
/// irrelevant to what these tests assert — nonce/confirm/reorg behavior).
pub const RECIPIENT: Address = Address::new([0xbb; 20]);

pub struct Localnet {
    _anvil: AnvilInstance,
    pub wallet: Arc<Wallet>,
    /// Raw alloy provider for chain control (mining, reorg, external txs).
    pub control: DynProvider,
    pub account: Address,
}

impl Localnet {
    /// Spawn a fresh anvil + a `Wallet` over account 0. `None` when anvil is absent.
    pub async fn spawn() -> Option<Localnet> {
        Self::spawn_with_confirmations(1).await
    }

    /// As [`spawn`](Self::spawn) but with a chosen confirmation depth (reorg tests
    /// need a tentative `Mined` window, so a depth > 1).
    pub async fn spawn_with_confirmations(confirmations: u64) -> Option<Localnet> {
        // `--slots-in-an-epoch 1` makes anvil's `finalized` tag advance quickly (it
        // otherwise lags ~64 blocks), so the executor's finalized-anchored confirm
        // settles within a few mined blocks.
        let anvil = Anvil::new()
            .arg("--slots-in-an-epoch")
            .arg("1")
            .try_spawn()
            .ok()?;
        let url = anvil.endpoint_url();

        let signer = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, 0).ok()?;
        let account = signer.address();
        let transport = Transport::single(url.clone()).ok()?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let policy: Arc<dyn PolicyEngine> = Arc::new(DefaultPolicyEngine::new(
            vec![Box::new(TargetAllowlist::new([RECIPIENT]))],
            clock,
        ));

        let wallet = Wallet::builder(Arc::new(transport), Arc::new(signer), policy)
            .confirmations(confirmations)
            .bump_timeout(0)
            .gas_ceiling(u128::MAX)
            .build();

        let control = ProviderBuilder::new().connect_http(url).erased();
        Some(Localnet {
            _anvil: anvil,
            wallet: Arc::new(wallet),
            control,
            account,
        })
    }

    /// A value-transfer intent from this wallet's account to the allowlisted recipient.
    pub fn intent(&self, value: u64) -> TxIntent {
        self.intent_wei(U256::from(value))
    }

    /// As [`intent`](Self::intent) but with an arbitrary wei value (e.g. an
    /// over-balance amount to trip the estimate gate).
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
}
