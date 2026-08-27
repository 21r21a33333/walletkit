//! Confirmation-safety fault injection — the one property that matters most: walletkit must
//! **never** report a false `Confirmed`. A false confirm tells the caller "your tx settled"
//! when it didn't; everything downstream acts on a lie, silently and irreversibly.
//!
//! These drive the real stack (Wallet → executor → adapters) against a real anvil, but with
//! the transport wrapped in a [`FaultRpc`](support::fault::FaultRpc) that lies on the two
//! reads a false confirm hinges on — reproducing the one condition an honest node cannot: a
//! receipt served from a block the chain has reorged away, while the head keeps advancing.
//! The executor's block-hash anchoring and lagging-head guards must hold end-to-end here,
//! not just in the pure-function unit tests. Skips cleanly when anvil is absent.
//!
//! Each test asserts **both** directions: with the fault active the tx never confirms, and
//! once the fault clears the *same* tx confirms — proving it was confirmable all along and
//! the guard (not an unconfirmable tx) is what held it.

mod support;

use std::sync::Arc;
use support::{Backend, Localnet, fault::Faults};
use walletkit::Wallet;
use walletkit::core::wallet::{HandleId, TxStatus};

const CONFIRMATIONS: u64 = 2;

fn is_confirmed(status: &Option<TxStatus>) -> bool {
    matches!(status, Some(TxStatus::Confirmed { .. }))
}

/// Mine + tick until `id` confirms or `rounds` elapse; returns the final status.
async fn settle(net: &Localnet, wallet: &Wallet, id: HandleId, rounds: u32) -> Option<TxStatus> {
    let mut status = wallet.status(id).await.expect("status");
    for _ in 0..rounds {
        if is_confirmed(&status) {
            break;
        }
        net.mine(2).await;
        wallet.tick().await.expect("tick");
        status = wallet.status(id).await.expect("status");
    }
    status
}

/// Send a confirmable tx over a fault wallet and mine it well past the finality threshold,
/// so only an injected lie can keep it from confirming. Returns the wallet, faults, handle.
async fn armed(net: &Localnet) -> (Arc<Wallet>, Arc<Faults>, HandleId) {
    let faults = Arc::new(Faults::default());
    let wallet = net.fault_wallet(&faults);
    let handle = wallet.send(&net.intent(1_000)).await.expect("send");
    assert_eq!(handle.status, TxStatus::Sent);
    net.mine(6).await; // genuinely confirmable before any confirm tick runs
    (wallet, faults, handle.id)
}

/// Tick `rounds` times with the fault active, asserting the tx never reaches `Confirmed`.
async fn assert_never_confirms(net: &Localnet, wallet: &Wallet, id: HandleId, rounds: u32) {
    for _ in 0..rounds {
        net.mine(2).await;
        wallet.tick().await.expect("tick");
        let status = wallet.status(id).await.expect("status");
        assert!(
            !is_confirmed(&status),
            "false confirm under injected fault: {status:?}"
        );
    }
}

async fn assert_recovers(net: &Localnet, wallet: &Wallet, id: HandleId) {
    let status = settle(net, wallet, id, 8).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "expected recovery to Confirmed, got {status:?}"
    );
}

/// A receipt served from a block whose canonical hash no longer matches (stale fork after a
/// reorg) must be rejected by anchoring — never a false confirm — then confirm once honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_fork_receipt_never_false_confirms_then_recovers() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 1, CONFIRMATIONS).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let (wallet, faults, id) = armed(&net).await;

    faults.corrupt_block_hash(true);
    assert_never_confirms(&net, &wallet, id, 6).await;

    faults.corrupt_block_hash(false);
    assert_recovers(&net, &wallet, id).await;
}

/// A node that cannot resolve the receipt's block (`block_hash` → `None`) is un-anchorable;
/// the executor must treat it as no-evidence, never a confirm, then confirm once resolvable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unanchorable_receipt_is_ignored_then_recovers() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 2, CONFIRMATIONS).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let (wallet, faults, id) = armed(&net).await;

    faults.block_hash_none(true);
    assert_never_confirms(&net, &wallet, id, 6).await;

    faults.block_hash_none(false);
    assert_recovers(&net, &wallet, id).await;
}

/// A lagging head (a node reporting an old `block_number`) must not drive a premature
/// confirm — the cycle is skipped — then confirm once the head is honest again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lagging_head_never_confirms_early_then_recovers() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 3, CONFIRMATIONS).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let (wallet, faults, id) = armed(&net).await;

    // Pin the head to block 1 — below both the tx's block and the finalized height — so the
    // executor sees a stale/lagging node and skips the cycle rather than confirming.
    faults.freeze_head(1);
    assert_never_confirms(&net, &wallet, id, 6).await;

    faults.freeze_head(0);
    assert_recovers(&net, &wallet, id).await;
}
