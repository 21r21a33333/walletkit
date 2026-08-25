//! Read an account's native and ERC-20 balances over the resilient transport.
//!
//! ```sh
//! WALLETKIT_RPC=https://… cargo run --example read_balance -- 0xAccount [0xToken]
//! ```

use walletkit::adapters::{RpcReadClient, Transport};
use walletkit::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("WALLETKIT_RPC")?;
    let account: Address = std::env::args()
        .nth(1)
        .ok_or("usage: read_balance <account> [token]")?
        .parse()?;

    let read = RpcReadClient::new(Transport::url(rpc.parse()?)?.provider());
    println!(
        "native: {} ETH",
        format_ether(read.native_balance(account).await?)
    );

    if let Some(token) = std::env::args().nth(2) {
        let token: Address = token.parse()?;
        let meta = read.erc20_metadata(token).await?;
        let bal = read.erc20_balance(token, account).await?;
        println!("{}: {} (raw, {} decimals)", meta.symbol, bal, meta.decimals);
    }
    Ok(())
}
