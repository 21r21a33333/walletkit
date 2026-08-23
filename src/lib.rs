//! walletkit — a client-side ergonomic facade over [alloy] for wallet
//! infrastructure and transaction execution.
//!
//! Hexagonal layout (mirrors evm-executor): [`core`] holds the domain and the
//! object-safe ports ([`core::deps`]); [`adapters`] holds concrete
//! implementations behind those ports.
//!
//! [alloy]: https://github.com/alloy-rs

pub mod adapters;
pub mod core;
pub mod facade;

pub use facade::{Runner, Wallet, WalletBuilder, WalletError};

#[cfg(test)]
pub(crate) mod testutils;
