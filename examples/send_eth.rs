//! Send ETH through a policy-gated wallet, in a handful of lines.
//!
//! ```sh
//! WALLETKIT_RPC=https://… WALLETKIT_KEY=0x… WALLETKIT_CHAIN_ID=1 \
//!   cargo run --example send_eth -- 0xRecipient 0.001
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
        .ok_or("usage: send_eth <to> <eth>")?
        .parse()?;
    let eth = std::env::args().nth(2).unwrap_or_else(|| "0.001".into());
    let chain_id: u64 = std::env::var("WALLETKIT_CHAIN_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    // The recipient is the only allowed target — the guardrail stays explicit.
    let policy = DefaultPolicyEngine::new(
        vec![Box::new(TargetAllowlist::new([to]))],
        Arc::new(SystemClock),
    );
    let wallet = Wallet::connect_http(&rpc, LocalSigner::from_private_key(&key)?, policy)?;

    let intent = TxIntent::transfer(chain_id, wallet.account(), to, parse_ether(&eth)?);
    let handle = wallet.send(&intent).await?;
    println!("submitted {:?}", handle.id);
    Ok(())
}
