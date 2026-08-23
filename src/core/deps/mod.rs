//! Object-safe ports (`Arc<dyn _>`, `Send + Sync`, `#[async_trait]`) that adapters
//! implement. One file per port, each owning its own `{TraitName}Error`. Each
//! defines only the methods a Phase-1 consumer calls and reuses alloy data types
//! rather than inventing its own; the surface grows in later phases.
//!
pub mod account;
pub mod clock;
pub mod gas_oracle;
pub mod nonce_manager;
pub mod policy_engine;
pub mod rpc;
pub mod signer;
pub mod state_store;
pub mod submission;

pub use account::Account;
pub use clock::Clock;
pub use gas_oracle::{GasOracle, GasOracleError};
pub use nonce_manager::{NonceManager, NonceManagerError};
pub use policy_engine::{PolicyEngine, PolicyEngineError};
pub use rpc::{Rpc, RpcError};
pub use signer::{Signer, SignerError};
pub use state_store::{StateStore, StateStoreError, Versioned};
pub use submission::{SubmissionError, SubmissionStrategy};
