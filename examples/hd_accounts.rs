//! Generate a fresh HD seed and derive the first few accounts under it.
//!
//! ```sh
//! cargo run --example hd_accounts
//! ```

use walletkit::core::accounts::WordCount;
use walletkit::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A fresh 12-word seed (fail-closed OS CSPRNG). In a real app, back this up once.
    let manager = AccountManager::generate(WordCount::W12)?;
    for i in 0..5 {
        let account = manager.account(i)?;
        println!("{}\t{}", account.path, account.address);
    }
    Ok(())
}
