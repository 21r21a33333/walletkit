//! `AccountManager` discovery integration (hermetic embedded-anvil). We derive accounts from
//! a non-anvil seed (so they start empty on a fresh node), then mark specific indices "used"
//! with anvil cheats — `anvil_setNonce` for outbound activity, `anvil_setBalance` for a
//! receive-only address — and assert the gap-limit scan finds exactly those, that a used
//! index resets the gap run, and that `NonceOnly` misses the receive-only address.

mod support;

use alloy_primitives::U256;
use alloy_provider::Provider;
use std::sync::Arc;
use support::{Backend, Localnet};
use walletkit::adapters::{AccountManager, Transport};
use walletkit::core::accounts::{DiscoveryOpts, UsedPredicate};
use walletkit::core::deps::Rpc;

// A valid BIP-39 mnemonic that is NOT anvil's default, so its accounts are empty on a fresh
// node until we fund/txn them.
const SEED: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

#[tokio::test]
async fn discovers_used_accounts_and_stops_on_gap() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 0, 1).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let rpc: Arc<dyn Rpc> = Arc::new(Transport::url(net.endpoint()).unwrap());
    let chains = [rpc];

    let mgr = AccountManager::from_phrase(SEED).unwrap();
    let addr0 = mgr.account(0).unwrap().address;
    let addr2 = mgr.account(2).unwrap().address;

    // index 0: outbound activity (nonce > 0). index 2: receive-only (balance > 0, nonce 0).
    let _: () = net
        .control
        .raw_request("anvil_setNonce".into(), (addr0, U256::from(1)))
        .await
        .unwrap();
    let _: () = net
        .control
        .raw_request(
            "anvil_setBalance".into(),
            (addr2, U256::from(1_000_000_000_000_000_000u64)),
        )
        .await
        .unwrap();

    // NonceOrBalance finds both; the gap of unused 1,3,4,5 stops the scan after index 5.
    let found = mgr
        .discover(
            &chains,
            DiscoveryOpts {
                gap_limit: 3,
                used: UsedPredicate::NonceOrBalance,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let indices: Vec<u32> = found.accounts.iter().map(|a| a.index).collect();
    assert_eq!(indices, vec![0, 2], "gap run resets on the used index 2");
    assert!(
        !found.hit_max_index,
        "the gap ended the scan, not the bound"
    );
    assert!(!found.partial, "no RPC errors");

    // NonceOnly must MISS the receive-only index 2 (nonce 0) — the documented gap.
    let nonce_only = mgr
        .discover(
            &chains,
            DiscoveryOpts {
                gap_limit: 3,
                used: UsedPredicate::NonceOnly,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let indices: Vec<u32> = nonce_only.accounts.iter().map(|a| a.index).collect();
    assert_eq!(indices, vec![0], "NonceOnly sees only the outbound account");
}
