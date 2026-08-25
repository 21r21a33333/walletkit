//! Object-safe ports (`Arc<dyn _>`, `Send + Sync`, `#[async_trait]`) that adapters
//! implement. One file per port, each owning its own `{TraitName}Error`. Each defines
//! only the methods a consumer actually calls and reuses alloy data types rather than
//! inventing its own.
//!
pub mod clock;
pub mod ens;
pub mod gas_oracle;
pub mod nonce_manager;
pub mod policy_engine;
#[cfg(feature = "pricing")]
pub mod pricing;
pub mod read;
pub mod rpc;
pub mod signer;
pub mod state_store;
pub mod submission;

pub use clock::Clock;
pub use ens::{EnsError, EnsResolver};
pub use gas_oracle::{GasOracle, GasOracleError};
pub use nonce_manager::{NonceManager, NonceManagerError};
pub use policy_engine::{PolicyEngine, PolicyEngineError};
#[cfg(feature = "pricing")]
pub use pricing::{Currency, Price, PriceSource, PricingError, TokenMetadataSource};
pub use read::{AccountBalances, Erc20Metadata, ReadClient, ReadError, TokenBalance};
pub use rpc::{AccountActivity, Rpc, RpcError, Simulated};
pub use signer::{Signer, SignerError};
pub use state_store::{StateStore, StateStoreError, Versioned};
pub use submission::{SubmissionError, SubmissionStrategy};
