//! Convenience-constructor integration (hermetic embedded-anvil). `connect_http_dev` wires an
//! allow-all wallet in one call and must permit a tx to an arbitrary target; `connect_http`
//! with a strict policy must deny the same tx before it is signed or broadcast. Also
//! exercises the `prelude` glob (`Wallet`, `TxIntent`, `Address`, `parse_ether`).

mod support;

use std::sync::Arc;
use support::{Backend, Localnet};
use walletkit::WalletKitError;
use walletkit::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};
use walletkit::adapters::{LocalSigner, SystemClock};
use walletkit::prelude::*;

const MNEMONIC: &str = "test test test test test test test test test test test junk";

#[tokio::test]
async fn connect_http_dev_allows_any_target_but_strict_policy_denies() {
    let Some(net) = Localnet::spawn_on(Backend::InMemory, 0, 1).await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let endpoint = net.endpoint();
    let to = Address::from([0x99; 20]); // arbitrary, NOT an allowlisted target
    let value = parse_ether("0.001").unwrap();

    // Dev wallet (allow-all): a transfer to an arbitrary target is permitted and broadcasts.
    let dev_signer = LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap();
    let dev = Wallet::connect_http_dev(endpoint.as_str(), dev_signer).unwrap();
    let intent = TxIntent::transfer(net.chain_id(), dev.account(), to, value);
    dev.send(&intent)
        .await
        .expect("allow-all dev policy should permit any target");

    // Strict wallet (empty allowlist): the same shape is denied by policy, before signing.
    let strict_signer = LocalSigner::from_mnemonic(MNEMONIC, 1).unwrap();
    let strict = Wallet::connect_http(
        endpoint.as_str(),
        strict_signer,
        DefaultPolicyEngine::new(
            vec![Box::new(TargetAllowlist::new([]))],
            Arc::new(SystemClock),
        ),
    )
    .unwrap();
    let denied = TxIntent::transfer(net.chain_id(), strict.account(), to, value);
    assert!(matches!(
        strict.send(&denied).await,
        Err(WalletKitError::Policy(_))
    ));
}

#[tokio::test]
async fn connect_http_rejects_a_malformed_url() {
    let err = Wallet::connect_http_dev(
        "not a url",
        LocalSigner::from_mnemonic(MNEMONIC, 0).unwrap(),
    );
    assert!(matches!(err, Err(WalletKitError::Connect(_))));
}
