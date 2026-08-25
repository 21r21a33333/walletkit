//! A curated glob-import for the common path: `use walletkit::prelude::*;`.
//!
//! It brings the facade, the object-safe port **traits** (whose methods won't resolve unless
//! the trait is in scope — the main papercut of a trait-heavy API), the intent/account entry
//! points, and the re-exported value/unit types. Deliberately small: adapter structs
//! (`Transport`, `LocalSigner`, `DefaultPolicyEngine`) are named at construction sites via
//! [`walletkit::adapters`](crate::adapters), not globbed here.

pub use crate::adapters::AccountManager;
pub use crate::core::deps::{PolicyEngine, ReadClient, Rpc, Signer};
pub use crate::core::wallet::TxIntent;
pub use crate::types::*;
pub use crate::units::*;
pub use crate::{Wallet, WalletBuilder};
