//! Localnet integration tests — the whole stack (Wallet → pipeline + executor →
//! adapters) against a real anvil with real transactions. Each test spawns a fresh
//! node and skips cleanly when anvil isn't installed.

mod support;

use support::Localnet;
use walletkit::core::wallet::{HandleId, TxStatus};

/// Spawn a localnet (optionally at a given confirmation depth) or skip the test when
/// anvil isn't on PATH — so the suite is a clean no-op without Foundry.
macro_rules! localnet {
    () => {
        localnet!(@ Localnet::spawn())
    };
    ($confirmations:expr) => {
        localnet!(@ Localnet::spawn_with_confirmations($confirmations))
    };
    (@ $spawn:expr) => {
        match $spawn.await {
            Some(net) => net,
            None => {
                eprintln!("skipping: anvil not found on PATH");
                return;
            }
        }
    };
}

fn is_terminal(status: &Option<TxStatus>) -> bool {
    matches!(
        status,
        Some(TxStatus::Confirmed { .. }) | Some(TxStatus::Failed { .. }) | Some(TxStatus::Replaced)
    )
}

/// Mine + tick up to `rounds` times until `id` reaches a terminal state (I3: a tracked
/// tx must always settle, never hang). Returns the final status.
async fn settle(net: &Localnet, id: HandleId, rounds: u32) -> Option<TxStatus> {
    let mut status = net.wallet.status(id).await.expect("status");
    for _ in 0..rounds {
        if is_terminal(&status) {
            break;
        }
        net.mine(2).await;
        net.wallet.tick().await.expect("tick");
        status = net.wallet.status(id).await.expect("status");
    }
    status
}

/// Settle `id` and assert it confirmed.
async fn assert_confirms(net: &Localnet, id: HandleId) {
    let status = settle(net, id, 8).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "expected Confirmed, got {status:?}"
    );
}

#[tokio::test]
async fn single_tx_confirms() {
    let net = localnet!();
    let handle = net.wallet.send(&net.intent(1_000)).await.expect("send");
    assert_eq!(handle.status, TxStatus::Sent);
    assert_confirms(&net, handle.id).await;
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
    assert_confirms(&net, handle.id).await;
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

    for handle in &handles {
        assert_confirms(&net, handle.id).await;
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
    assert_confirms(&net, recovered.id).await;
}

#[tokio::test]
async fn stuck_tx_is_bumped_then_confirms() {
    let net = localnet!();
    net.no_auto_mine().await;

    // Low-fee send sits pending (mining off).
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    let sent = net
        .wallet
        .handle(handle.id)
        .await
        .expect("handle")
        .expect("present");
    assert_eq!(sent.broadcasts.len(), 1);

    // A tick escalates the stuck tx (bump_timeout 0) -> same-nonce RBF -> a 2nd
    // broadcast, which anvil accepts as a replacement.
    net.wallet.tick().await.expect("tick-bump");
    let bumped = net
        .wallet
        .handle(handle.id)
        .await
        .expect("handle")
        .expect("present");
    assert!(
        bumped.broadcasts.len() >= 2,
        "expected a bump broadcast, got {}",
        bumped.broadcasts.len()
    );

    assert_confirms(&net, handle.id).await;
}

#[tokio::test]
async fn reorg_unmines_without_false_confirm_then_recovers() {
    // Depth 3 keeps the mined tx tentative (not terminal) so the reorg can act on it.
    let net = localnet!(3);

    // Mine our tx; it's tentatively Mined (depth 3 not yet met).
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    net.wallet.tick().await.expect("tick-mine");
    assert!(
        matches!(
            net.wallet.status(handle.id).await.expect("status"),
            Some(TxStatus::Mined { .. })
        ),
        "expected tentative Mined, got {:?}",
        net.wallet.status(handle.id).await.expect("status")
    );

    // Reorg past it (auto-mine off so the dropped tx stays observably un-mined).
    net.no_auto_mine().await;
    net.reorg(1).await;
    net.wallet.tick().await.expect("tick-reorg");
    let after = net.wallet.status(handle.id).await.expect("status");
    assert!(
        !is_terminal(&after),
        "a reorg must never produce a false terminal, got {after:?}"
    );

    // Recovery: rebroadcast + re-mine + confirm (settle is robust to the un-mine landing
    // as Sent or being held tentative on a stale read).
    assert_confirms(&net, handle.id).await;
}

#[tokio::test]
async fn restart_reconciles_a_tx_mined_during_downtime() {
    let net = localnet!();

    // Send, then the tx mines while the original wallet never ticks ("downtime").
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    net.mine(3).await;

    // Restart: a fresh wallet over the SAME store recovers and confirms it from the
    // persisted handle in a single tick — no rebroadcast needed since it already mined.
    let restarted = net.rebuild_wallet();
    restarted.tick().await.expect("tick after restart");
    assert!(
        matches!(
            restarted.status(handle.id).await.expect("status"),
            Some(TxStatus::Confirmed { .. })
        ),
        "restarted wallet should reconcile the mined tx to Confirmed"
    );
}

#[tokio::test]
async fn every_tx_settles_within_bounded_ticks() {
    let net = localnet!();
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    let status = settle(&net, handle.id, 10).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "tx must settle to a terminal state within 10 rounds, got {status:?}"
    );
}
