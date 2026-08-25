//! Resolve an ENS name to its address (forward), over the resilient transport.
//!
//! ```sh
//! WALLETKIT_RPC=https://mainnet… cargo run --example resolve_ens -- vitalik.eth
//! ```

use walletkit::adapters::{RpcEnsResolver, Transport};
use walletkit::core::deps::EnsResolver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("WALLETKIT_RPC")?;
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "vitalik.eth".into());

    let ens = RpcEnsResolver::new(Transport::url(rpc.parse()?)?.provider());
    match ens.resolve_name(&name).await? {
        Some(addr) => println!("{name} -> {addr}"),
        None => println!("{name} has no address record"),
    }
    Ok(())
}
