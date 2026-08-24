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
//! [alloy]: https://github.com/alloy-rs

pub mod adapters;
pub mod core;
pub mod error;
pub mod facade;

pub(crate) mod obs;

pub use error::{ErrorKind, WalletKitError};
pub use facade::{Runner, Wallet, WalletBuilder};

#[cfg(test)]
pub(crate) mod testutils;
