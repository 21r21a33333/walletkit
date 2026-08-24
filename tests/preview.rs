//! `Rpc` simulation surface over embedded anvil: `eth_call` returns a contract's revert
//! data as a normal `Simulated::Reverted` (not an `Err`), and `create_access_list`
//! resolves. Skips cleanly when anvil is unavailable. Extended with `dry_run` scenarios in
//! the preview task.

mod support;

use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::SolCall;
use support::{Backend, Localnet, MockErc20};
use walletkit::adapters::Transport;
use walletkit::core::deps::{Rpc, Simulated};
use walletkit::core::wallet::{RevertReason, SimOutcome, TxIntent};

#[tokio::test]
async fn eth_call_distinguishes_return_from_revert() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 0, 1).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let token = net.deploy_mock_erc20(0).await;
    let caller = net.account_at(0);
    let transport = Transport::url(net.endpoint()).unwrap();

    // A reverting view yields its revert data (`Error("nope")`) as a successful simulation.
    let revert_req = TransactionRequest {
        from: Some(caller),
        to: Some(TxKind::Call(token)),
        input: TransactionInput::new(MockErc20::revertWithCall {}.abi_encode().into()),
        ..Default::default()
    };
    assert!(matches!(
        transport.call(&revert_req).await.unwrap(),
        Simulated::Reverted(_)
    ));

    // A call to an EOA succeeds with empty return data.
    let ok_req = TransactionRequest {
        from: Some(caller),
        to: Some(TxKind::Call(net.account_at(1))),
        ..Default::default()
    };
    assert!(matches!(
        transport.call(&ok_req).await.unwrap(),
        Simulated::Returned(_)
    ));

    // create_access_list resolves for a plain transfer (an empty list, no error).
    let access = transport.create_access_list(&ok_req).await.unwrap();
    assert!(access.error.is_none());
}

#[tokio::test]
async fn dry_run_previews_success_and_decodes_revert() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 0, 1).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let token = net.deploy_mock_erc20(0).await;

    // A reverting call → a *successful* preview with a decoded `Error("nope")`, and no gas
    // estimate (a reverting tx has none). dry_run never signs or touches the store.
    let revert_intent = TxIntent {
        chain_id: net.chain_id(),
        account: net.account,
        to: TxKind::Call(token),
        value: U256::ZERO,
        input: MockErc20::revertWithCall {}.abi_encode().into(),
        purpose: None,
    };
    let preview = net.wallet.dry_run(&revert_intent).await.unwrap();
    assert!(
        matches!(preview.outcome, SimOutcome::Revert(RevertReason::Error(ref s)) if s == "nope")
    );
    assert!(preview.gas_estimate.is_none());

    // A plain value transfer → Success with a gas estimate at/above the 21k floor.
    let ok_intent = TxIntent {
        chain_id: net.chain_id(),
        account: net.account,
        to: TxKind::Call(net.account_at(6)),
        value: U256::from(1u64),
        input: Default::default(),
        purpose: None,
    };
    let preview = net.wallet.dry_run(&ok_intent).await.unwrap();
    assert!(matches!(preview.outcome, SimOutcome::Success));
    assert!(preview.gas_estimate.unwrap() >= 21_000);
}
