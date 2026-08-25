//! Preview a tx two orthogonal ways before sending: `dry_run` (what would happen on-chain)
//! and `validate` (would the policy allow it).
//!
//! ```sh
//! WALLETKIT_RPC=https://… WALLETKIT_KEY=0x… \
//!   cargo run --example preview_and_validate -- 0xRecipient
//! ```

use std::sync::Arc;
use walletkit::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};
use walletkit::adapters::{LocalSigner, SystemClock};
use walletkit::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("WALLETKIT_RPC")?;
    let key = std::env::var("WALLETKIT_KEY")?;
    let to: Address = std::env::args()
        .nth(1)
        .ok_or("usage: preview_and_validate <to>")?
        .parse()?;

    // An empty allowlist denies everything — so `validate` reports WouldDeny while `dry_run`
    // still simulates the transfer against the chain. Neither signs or broadcasts.
    let policy = DefaultPolicyEngine::new(
        vec![Box::new(TargetAllowlist::new([]))],
        Arc::new(SystemClock),
    );
    let wallet = Wallet::connect_http(&rpc, LocalSigner::from_private_key(&key)?, policy)?;
    let intent = TxIntent::transfer(1, wallet.account(), to, parse_ether("0.001")?);

    println!("on-chain preview: {:?}", wallet.dry_run(&intent).await?);
    println!("policy preview:   {:?}", wallet.validate(&intent).await?);
    Ok(())
}
