//! `EnsResolver` integration against exact real mainnet state at a pinned block (forking
//! pins the ENS records). Opt in via `WALLETKIT_FORK_RPC` (an archive RPC — a free option is
//! `https://eth.drpc.org`); skips cleanly when unset or the fork is unavailable. The
//! None-mapping logic has an always-run unit test in the adapter.

mod support;

use alloy_primitives::address;
use support::ForkedNet;
use walletkit::adapters::{RpcEnsResolver, Transport};
use walletkit::core::deps::EnsResolver;

/// Records below were read from mainnet at exactly this height, so they don't rot. Chosen
/// after the ENS Universal Resolver (`0xeeee…eeee`, which `alloy-ens` `resolve_name` calls)
/// was deployed (~block 23.1M).
const FORK_BLOCK: u64 = 23_500_000;

#[tokio::test]
async fn resolves_forward_reverse_verified_and_maps_missing_to_none() {
    let Ok(rpc) = std::env::var("WALLETKIT_FORK_RPC") else {
        eprintln!("skipping: set WALLETKIT_FORK_RPC to an archive mainnet RPC to run");
        return;
    };
    let Some(net) = ForkedNet::pin(&rpc, FORK_BLOCK).await else {
        eprintln!("skipping: anvil or archive fork at block {FORK_BLOCK} unavailable");
        return;
    };
    let ens = RpcEnsResolver::new(Transport::url(net.endpoint()).unwrap().provider());

    let vitalik = address!("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045");

    // Forward resolution (exact).
    assert_eq!(
        ens.resolve_name("vitalik.eth").await.unwrap(),
        Some(vitalik)
    );

    // Reverse resolution — forward-verified: the primary name resolves back to the address.
    assert_eq!(
        ens.reverse_lookup(vitalik).await.unwrap().as_deref(),
        Some("vitalik.eth")
    );

    // An unregistered name (no resolver) is a legitimate empty result, not an error.
    assert_eq!(
        ens.resolve_name("thisnamedoesnotexist99887766.eth")
            .await
            .unwrap(),
        None
    );

    // The text/avatar path resolves without error (the record may be set or not).
    ens.avatar("vitalik.eth").await.unwrap();
}
