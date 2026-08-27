// `--cfg docsrs` (set by docs.rs) turns on per-item `doc(cfg(...))` feature badges.
#![cfg_attr(docsrs, feature(doc_cfg))]
// Doc links must resolve to public items — no dangling or private-item links escape review.
#![deny(rustdoc::broken_intra_doc_links, rustdoc::private_intra_doc_links)]
// The no-panic house rule, enforced. Relaxed under `cfg(test)` so unit tests may `unwrap`.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
// Every public item must carry documentation.
#![deny(missing_docs)]

//! walletkit — a client-side ergonomic facade over [alloy] for wallet
//! infrastructure and transaction execution.
//!
//! Hexagonal layout: [`core`] holds the domain and the object-safe ports
//! ([`core::deps`]); [`adapters`] holds the concrete implementations behind those ports.
//! You wire adapters into the [`Wallet`] facade and drive everything through it.
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
//! # Feature flags
//!
//! | Feature | Default | Enables |
//! |---|:---:|---|
//! | `tracing` | ✓ | `tracing` spans/events on key paths (redacted). Off → the shim compiles to no-ops. |
//! | `redb` | ✓ | The embedded `RedbStateStore` durable backend. |
//! | `postgres` | | The networked `PostgresStateStore` backend. |
//! | `pricing` | | The `pricing` seam: token-list metadata + Chainlink prices. |
//! | `policy-moonpay` | | The MoonPay Open Wallet Standard engine with a sandboxed `wasip1` plugin runner. |
//!
//! With `--no-default-features` the crate builds down to the ports plus the
//! [`InMemoryStateStore`](adapters::InMemoryStateStore) — the minimal dependency surface.
//!
//! # Design
//!
//! See the [`SPEC.md`](https://github.com/21r21a33333/walletkit/blob/main/SPEC.md) for the
//! architecture, the phase roadmap, and the cross-cutting invariants (policy→sign binding,
//! nonce fencing, reorg-aware finality).
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
