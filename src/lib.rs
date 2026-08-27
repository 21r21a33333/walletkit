// `--cfg docsrs` (set by docs.rs) turns on per-item `doc(cfg(...))` feature badges.
#![cfg_attr(docsrs, feature(doc_cfg))]
// Doc links must resolve to public items — no dangling or private-item links escape review.
#![deny(rustdoc::broken_intra_doc_links, rustdoc::private_intra_doc_links)]
// The no-panic house rule, enforced. Relaxed under `cfg(test)` so unit tests may `unwrap`.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! walletkit — a client-side ergonomic facade over [alloy] for wallet
//! infrastructure and transaction execution.
//!
//! Hexagonal layout (mirrors evm-executor): [`core`] holds the domain and the
//! object-safe ports ([`core::deps`]); [`adapters`] holds concrete
//! implementations behind those ports.
//!
//! Beyond sending and tracking transactions, it exposes read-only surfaces over the same
//! resilient transport: [`ReadClient`](core::deps::ReadClient) (balances/metadata/allowances,
//! Multicall3-batched), [`Wallet::dry_run`] → [`TxPreview`](core::wallet::TxPreview) (RPC-only
//! simulation with decoded revert reasons), and [`EnsResolver`](core::deps::EnsResolver).
//! Token metadata + prices are an opt-in `pricing` feature.
//!
//! [`AccountManager`](adapters::AccountManager) adds HD key management: BIP-39 seed
//! generation/restore, multi-account BIP-44/Ledger-Live derivation, watch-only account
//! xpubs, counterfactual [`predict_address`](core::accounts::predict_address), gap-limit
//! account discovery (one batched round-trip per window), and encrypted keystore export.
//!
//! Start with [`Wallet::connect_http`] and `use walletkit::prelude::*;` — the [`prelude`]
//! brings the facade, the port traits, and the common alloy value/unit types into scope.
//!
//! # Quickstart
//!
//! ```no_run
//! use std::sync::Arc;
//! use walletkit::prelude::*;
//! use walletkit::adapters::{LocalSigner, SystemClock};
//! use walletkit::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};
//!
//! # async fn quickstart() -> Result<(), walletkit::WalletKitError> {
//! let to = Address::from([0x22; 20]);
//! let signer = LocalSigner::from_private_key("0x59c6…").unwrap();
//! // The recipient is the only allowed target — the guardrail stays explicit.
//! let policy = DefaultPolicyEngine::new(
//!     vec![Box::new(TargetAllowlist::new([to]))],
//!     Arc::new(SystemClock),
//! );
//! let wallet = Wallet::connect_http("http://localhost:8545", signer, policy)?;
//!
//! let intent = TxIntent::transfer(1, wallet.account(), to, parse_ether("0.01").unwrap());
//! let handle = wallet.send(&intent).await?;
//! # Ok(())
//! # }
//! ```
//!
//! [alloy]: https://github.com/alloy-rs

pub mod adapters;
pub mod core;
pub mod error;
pub mod facade;
pub mod prelude;
pub mod types;
pub mod units;

pub(crate) mod obs;

pub use error::{ErrorKind, WalletKitError};
pub use facade::{Runner, Wallet, WalletBuilder};

#[cfg(test)]
pub(crate) mod testutils;
