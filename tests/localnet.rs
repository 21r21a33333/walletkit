//! Localnet integration tests — the whole stack (Wallet → pipeline + executor →
//! adapters) against a real anvil with real transactions. Each test spawns a fresh
//! node and skips cleanly when anvil isn't installed.

mod support;

use support::Localnet;
use walletkit::core::wallet::TxStatus;

/// Spawn a localnet or skip the test if anvil isn't on PATH.
macro_rules! localnet {
    () => {
        match Localnet::spawn().await {
            Some(net) => net,
            None => {
                eprintln!("skipping: anvil not found on PATH");
                return;
            }
        }
    };
}

#[tokio::test]
async fn single_tx_confirms() {
    let net = localnet!();
    let handle = net.wallet.send(&net.intent(1_000)).await.expect("send");
    assert_eq!(handle.status, TxStatus::Sent);

    // anvil auto-mines the tx; mine a couple more so it's final under either finality
    // mode (finalized-tag or depth>=1).
    net.mine(2).await;
    net.wallet.tick().await.expect("tick");

    let status = net.wallet.status(handle.id).await.expect("status");
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "expected Confirmed, got {status:?}"
    );
}

#[tokio::test]
async fn overspend_rejects_and_recycles_the_nonce() {
    use alloy_primitives::U256;
    use walletkit::WalletError;
    use walletkit::core::wallet::TransactionManagerError;

    let net = localnet!();
    // anvil estimates gas without a balance check, so an over-balance transfer is
    // rejected deterministically at submit (insufficient funds), not at estimate. The
    // pipeline terminalizes the handle and *recycles* the nonce.
    let err = net
        .wallet
        .send(&net.intent_wei(U256::MAX))
        .await
        .expect_err("overspend must be rejected");
    assert!(
        matches!(
            err,
            WalletError::Send(TransactionManagerError::Submission(_))
        ),
        "expected a deterministic submit reject, got {err:?}"
    );

    // The recycled nonce (0) is reused by the next valid send, which confirms — proving
    // a rejected tx leaves no nonce gap.
    let handle = net
        .wallet
        .send(&net.intent(1))
        .await
        .expect("send after reject");
    assert_eq!(
        handle.nonce, 0,
        "rejected nonce must be recycled, not skipped"
    );
    net.mine(2).await;
    net.wallet.tick().await.expect("tick");
    assert!(matches!(
        net.wallet.status(handle.id).await.expect("status"),
        Some(TxStatus::Confirmed { .. })
    ));
}
