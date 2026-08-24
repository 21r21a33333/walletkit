//! `Rpc` simulation surface over embedded anvil: `eth_call` returns a contract's revert
//! data as a normal `Simulated::Reverted` (not an `Err`), and `create_access_list`
//! resolves. Skips cleanly when anvil is unavailable. Extended with `dry_run` scenarios in
//! the preview task.

mod support;

use alloy_primitives::TxKind;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::SolCall;
use support::{Backend, Localnet, MockErc20};
use walletkit::adapters::Transport;
use walletkit::core::deps::{Rpc, Simulated};

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
