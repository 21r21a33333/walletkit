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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batch_gapless_nonces_and_all_confirm() {
    let net = localnet!();
    let n = 8u64;

    // Fire N sends at once — the CAS nonce allocator must hand out 0..n distinct,
    // gapless nonces under real concurrent submission.
    let mut tasks = Vec::new();
    for i in 0..n {
        let wallet = net.wallet.clone();
        let intent = net.intent(i);
        tasks.push(tokio::spawn(async move { wallet.send(&intent).await }));
    }
    let mut handles = Vec::new();
    for task in tasks {
        handles.push(task.await.expect("join").expect("send"));
    }

    let mut nonces: Vec<u64> = handles.iter().map(|h| h.nonce).collect();
    nonces.sort_unstable();
    assert_eq!(nonces, (0..n).collect::<Vec<_>>(), "gapless, unique nonces");

    // All must mine and confirm.
    net.mine(3).await;
    net.wallet.tick().await.expect("tick");
    for handle in &handles {
        assert!(
            matches!(
                net.wallet.status(handle.id).await.expect("status"),
                Some(TxStatus::Confirmed { .. })
            ),
            "nonce {} not confirmed",
            handle.nonce
        );
    }
}

#[tokio::test]
async fn external_nonce_steal_is_recovered() {
    let net = localnet!();
    net.no_auto_mine().await;

    // Our tx grabs nonce 0 and sits in the pool.
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    assert_eq!(handle.nonce, 0);

    // A higher-fee foreign tx (same key, out of band) at nonce 0 replaces ours in the
    // pool, then mines — consuming our nonce with a hash that isn't ours.
    net.steal_nonce(0).await;
    net.mine(3).await;

    net.wallet.tick().await.expect("tick");
    let status = net.wallet.status(handle.id).await.expect("status");
    assert!(
        matches!(
            status,
            Some(TxStatus::Replacing { .. }) | Some(TxStatus::Replaced)
        ),
        "expected Replacing/Replaced, got {status:?}"
    );

    // The allocator reconciled forward; a fresh send takes nonce 1 and confirms.
    let recovered = net
        .wallet
        .send(&net.intent(2))
        .await
        .expect("send after steal");
    assert_eq!(
        recovered.nonce, 1,
        "allocator must reconcile past the stolen nonce"
    );
    net.mine(3).await;
    net.wallet.tick().await.expect("tick2");
    assert!(matches!(
        net.wallet.status(recovered.id).await.expect("status2"),
        Some(TxStatus::Confirmed { .. })
    ));
}
