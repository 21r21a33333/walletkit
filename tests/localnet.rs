//! Localnet integration tests — the whole stack (Wallet → pipeline + executor → adapters)
//! against a real anvil with real transactions, run as a matrix over every storage backend.
//! Each scenario becomes one `#[tokio::test]` per backend (`single_tx_confirms::in_memory`,
//! `::redb`, `::postgres`); each spawns a fresh node + store and skips cleanly when anvil or
//! the backend is absent, so the suite is a no-op without Foundry/Postgres.

mod support;

use std::future::Future;
use support::{Backend, Localnet};
use walletkit::core::wallet::{HandleId, TxStatus};

/// One `#[tokio::test]` per (scenario × backend). A scenario is an `async fn(Localnet)`; the
/// matrix wires a fresh anvil + the backend's store and skips when either is unavailable.
/// Each scenario pins a distinct funded anvil account so runs over a shared Postgres — where
/// state is keyed by account — never collide across scenarios.
macro_rules! localnet_matrix {
    ($( $scenario:ident { account: $acct:expr, confirmations: $conf:expr } ),+ $(,)?) => {
        $(
            mod $scenario {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn in_memory() {
                    run(Backend::InMemory, $acct, $conf, super::$scenario).await;
                }

                #[cfg(feature = "redb")]
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn redb() {
                    run(Backend::Redb, $acct, $conf, super::$scenario).await;
                }

                #[cfg(feature = "postgres")]
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn postgres() {
                    run(Backend::Postgres, $acct, $conf, super::$scenario).await;
                }
            }
        )+
    };
}

/// Spawn the backend and run `scenario`, or skip when the backend/anvil is unavailable.
async fn run<F, Fut>(backend: Backend, account: u32, confirmations: u64, scenario: F)
where
    F: FnOnce(Localnet) -> Fut,
    Fut: Future<Output = ()>,
{
    match Localnet::spawn_on(backend, account, confirmations).await {
        Some(net) => scenario(net).await,
        None => eprintln!("skipping: {backend:?} or anvil unavailable"),
    }
}

fn is_terminal(status: &Option<TxStatus>) -> bool {
    matches!(
        status,
        Some(
            TxStatus::Confirmed { .. }
                | TxStatus::Failed { .. }
                | TxStatus::Replaced
                | TxStatus::Dropped
        )
    )
}

/// Mine + tick up to `rounds` times until `id` settles (I3: a tracked tx always settles,
/// never hangs). Returns the final status.
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

async fn assert_confirms(net: &Localnet, id: HandleId) {
    let status = settle(net, id, 8).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "expected Confirmed, got {status:?}"
    );
}

async fn single_tx_confirms(net: Localnet) {
    let handle = net.wallet.send(&net.intent(1_000)).await.expect("send");
    assert_eq!(handle.status, TxStatus::Sent);
    assert_confirms(&net, handle.id).await;
}

async fn overspend_rejects_and_recycles_the_nonce(net: Localnet) {
    use alloy_primitives::U256;
    use walletkit::WalletKitError;

    // anvil estimates gas without a balance check, so an over-balance transfer is rejected
    // deterministically at submit (insufficient funds), not at estimate. The pipeline
    // terminalizes the handle and recycles the nonce.
    let err = net
        .wallet
        .send(&net.intent_wei(U256::MAX))
        .await
        .expect_err("overspend must be rejected");
    assert!(
        matches!(err, WalletKitError::Submission(_)),
        "expected a deterministic submit reject, got {err:?}"
    );

    // The recycled nonce (0) is reused by the next valid send — proving no nonce gap.
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

async fn concurrent_batch_gapless_nonces_and_all_confirm(net: Localnet) {
    let n = 8u64;

    // Fire N sends at once — the CAS allocator must hand out 0..n distinct, gapless nonces
    // under real concurrent submission.
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

async fn external_nonce_steal_is_recovered(net: Localnet) {
    net.no_auto_mine().await;

    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    assert_eq!(handle.nonce, 0);

    // A higher-fee foreign tx (same key, out of band) at nonce 0 replaces ours in the pool,
    // then mines — consuming our nonce with a hash that isn't ours.
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

async fn stuck_tx_is_bumped_then_confirms(net: Localnet) {
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

    // A tick escalates the stuck tx (bump_timeout 0) -> same-nonce RBF -> a 2nd broadcast,
    // which anvil accepts as a replacement.
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

async fn reorg_unmines_without_false_confirm_then_recovers(net: Localnet) {
    // Depth 3 (set by the matrix) keeps the mined tx tentative so the reorg can act on it.
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

    // Recovery: rebroadcast + re-mine + confirm.
    assert_confirms(&net, handle.id).await;
}

async fn restart_reconciles_a_tx_mined_during_downtime(net: Localnet) {
    // Send, then the tx mines while the original wallet never ticks ("downtime").
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    net.mine(3).await;

    // Restart: a fresh wallet over the same store reconciles from the persisted handle in one
    // tick — no rebroadcast needed since it already mined.
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

async fn cancel_settles_original_as_dropped(net: Localnet) {
    net.no_auto_mine().await;

    let target = net.wallet.send(&net.intent(1)).await.expect("send");
    assert_eq!(target.nonce, 0);

    let cancel = net.wallet.cancel(target.id).await.expect("cancel");
    assert_eq!(cancel.nonce, target.nonce);

    let cancel_status = settle(&net, cancel.id, 8).await;
    assert!(
        matches!(cancel_status, Some(TxStatus::Confirmed { .. })),
        "cancel Confirmed, got {cancel_status:?}"
    );
    // Dropped, not Replaced — a foreign displacement would settle Replaced.
    let target_status = settle(&net, target.id, 8).await;
    assert!(
        matches!(target_status, Some(TxStatus::Dropped)),
        "original Dropped, got {target_status:?}"
    );

    let fresh = net.wallet.send(&net.intent(2)).await.expect("fresh send");
    assert_eq!(fresh.nonce, 1);
    assert_confirms(&net, fresh.id).await;
}

async fn every_tx_settles_within_bounded_ticks(net: Localnet) {
    let handle = net.wallet.send(&net.intent(1)).await.expect("send");
    let status = settle(&net, handle.id, 10).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "tx must settle to a terminal state within 10 rounds, got {status:?}"
    );
}

localnet_matrix! {
    single_tx_confirms                              { account: 1, confirmations: 1 },
    overspend_rejects_and_recycles_the_nonce        { account: 2, confirmations: 1 },
    concurrent_batch_gapless_nonces_and_all_confirm { account: 3, confirmations: 1 },
    external_nonce_steal_is_recovered               { account: 4, confirmations: 1 },
    stuck_tx_is_bumped_then_confirms                { account: 5, confirmations: 1 },
    reorg_unmines_without_false_confirm_then_recovers { account: 6, confirmations: 3 },
    restart_reconciles_a_tx_mined_during_downtime   { account: 7, confirmations: 1 },
    every_tx_settles_within_bounded_ticks           { account: 8, confirmations: 1 },
    cancel_settles_original_as_dropped              { account: 9, confirmations: 1 },
}
