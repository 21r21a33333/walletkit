//! Gasless meta-transaction (ERC-2771) end-to-end against a real anvil and the **real** OZ
//! `ERC2771Forwarder` — the load-bearing proof the mock unit tests cannot give. Only an
//! on-chain `ECDSA.recover` inside the genuine forwarder can confirm walletkit signs the
//! `ForwardRequest` in the exact 65-byte `r‖s‖v` form (v ∈ {27, 28}) the forwarder expects; a
//! wrong `v` would recover the zero address and the whole meta-tx would silently mis-attribute
//! or revert.
//!
//! Model 1 under test: the user account signs the free `ForwardRequest` (through its own policy
//! gate), and a *separate* relayer account funds + sends the outer `execute()`, tracked by its
//! own executor. `tick()` drives both. Skips cleanly when anvil is absent.

mod support;

use std::sync::Arc;
use support::{Backend, Localnet, fault::Faults};
use walletkit::Wallet;
use walletkit::WalletKitError;
use walletkit::core::deps::SelfRelay;
use walletkit::core::wallet::{HandleId, TxIntent, TxStatus};

use alloy_primitives::{Bytes, U256};

const CONFIRMATIONS: u64 = 2;
/// `RecordingTarget.poke()` — records `_msgSender()` and bumps the counter.
const POKE: [u8; 4] = [0x18, 0x17, 0x83, 0x58];
/// `RecordingTarget.boom()` — records then reverts.
const BOOM: [u8; 4] = [0xa1, 0x69, 0xce, 0x09];

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

/// Mine + tick (driving *both* the user and relayer executors) until `id` settles or `rounds`
/// elapse; returns the final status.
async fn settle(net: &Localnet, wallet: &Wallet, id: HandleId, rounds: u32) -> Option<TxStatus> {
    let mut status = wallet.status(id).await.expect("status");
    for _ in 0..rounds {
        if is_terminal(&status) {
            break;
        }
        net.mine(2).await;
        wallet.tick().await.expect("tick");
        status = wallet.status(id).await.expect("status");
    }
    status
}

/// The happy path — and the one assertion that could only ever fail on a real chain: a real OZ
/// forwarder recovers our signature, runs the inner call attributing the *user* as
/// `_msgSender()`, and the relayer's executor confirms the outer `execute()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn self_relay_confirms_and_target_sees_the_user() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 1, CONFIRMATIONS).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let forwarder = net.deploy_erc2771_forwarder(0).await;
    let target = net.deploy_erc2771_target(0, forwarder).await;
    let user = net.account_at(1);
    let relayer = net.account_at(2);
    let wallet = net.gasless_wallet(1, 2, forwarder, CONFIRMATIONS);

    let intent = TxIntent::call(
        net.chain_id(),
        user,
        target,
        U256::ZERO,
        Bytes::from_static(&POKE),
    );
    let handle = wallet
        .send_gasless(&intent, SelfRelay::new())
        .await
        .expect("gasless send");
    assert_eq!(
        handle.account, relayer,
        "the outer execute() is the relayer's tx"
    );

    let status = settle(&net, &wallet, handle.id, 8).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "expected Confirmed, got {status:?}"
    );

    // The whole point of ERC-2771: the target saw the *user*, not the relayer, and the inner
    // call actually ran — which only holds if the forwarder recovered our signature correctly.
    assert_eq!(
        net.target_last_sender(target).await,
        user,
        "target must see the user as _msgSender, not the relayer"
    );
    assert_eq!(net.target_pokes(target).await, U256::from(1u64));
}

/// A deterministically-reverting inner call must be rejected by the pre-sign estimate gate —
/// so the relayer never wastes gas relaying a doomed request (fail-fast, no nonce consumed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reverting_inner_is_rejected_before_signing() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 3, CONFIRMATIONS).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let forwarder = net.deploy_erc2771_forwarder(0).await;
    let target = net.deploy_erc2771_target(0, forwarder).await;
    let user = net.account_at(3);
    let relayer = net.account_at(4);
    let wallet = net.gasless_wallet(3, 4, forwarder, CONFIRMATIONS);

    let intent = TxIntent::call(
        net.chain_id(),
        user,
        target,
        U256::ZERO,
        Bytes::from_static(&BOOM),
    );
    let err = wallet
        .send_gasless(&intent, SelfRelay::new())
        .await
        .expect_err("a reverting inner call must not be relayed");
    assert!(
        matches!(err, WalletKitError::Simulation { .. }),
        "expected a pre-sign Simulation reject, got {err:?}"
    );

    // Nothing was sent: the relayer never spent a nonce, and the target was never touched.
    assert_eq!(
        net.onchain_tx_count(relayer).await,
        0,
        "the relayer must not have sent the outer execute()"
    );
    assert_eq!(net.target_pokes(target).await, U256::ZERO);
}

/// Confirm-safety carries to the relayer's executor: a receipt served from a block whose
/// canonical hash no longer matches (a stale fork after a reorg) must never confirm the meta
/// handle — then the *same* tx confirms once the node is honest, proving the guard held it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reorg_of_the_outer_execute_never_falsely_confirms() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 5, CONFIRMATIONS).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let forwarder = net.deploy_erc2771_forwarder(0).await;
    let target = net.deploy_erc2771_target(0, forwarder).await;
    let user = net.account_at(5);
    let faults = Arc::new(Faults::default());
    let wallet = net.gasless_fault_wallet(5, 6, forwarder, CONFIRMATIONS, &faults);

    let intent = TxIntent::call(
        net.chain_id(),
        user,
        target,
        U256::ZERO,
        Bytes::from_static(&POKE),
    );
    let handle = wallet
        .send_gasless(&intent, SelfRelay::new())
        .await
        .expect("gasless send");
    net.mine(6).await; // genuinely confirmable before any confirm tick runs

    faults.corrupt_block_hash(true);
    for _ in 0..6 {
        net.mine(2).await;
        wallet.tick().await.expect("tick");
        let status = wallet.status(handle.id).await.expect("status");
        assert!(
            !matches!(status, Some(TxStatus::Confirmed { .. })),
            "false confirm of a meta-tx under an injected reorg: {status:?}"
        );
    }

    faults.corrupt_block_hash(false);
    let status = settle(&net, &wallet, handle.id, 8).await;
    assert!(
        matches!(status, Some(TxStatus::Confirmed { .. })),
        "expected recovery to Confirmed once honest, got {status:?}"
    );
    assert_eq!(net.target_last_sender(target).await, user);
}
