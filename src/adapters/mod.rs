//! Concrete implementations of the [`core::deps`](crate::core::deps) ports.
//! One (flat) module per adapter, added as each task lands.

pub mod clock;
pub mod gas_oracle;
pub mod nonce_store;
pub mod policy;
pub mod public_mempool;
pub mod signers;
pub mod transport;

pub use clock::SystemClock;
pub use gas_oracle::RpcGasOracle;
pub use nonce_store::{InMemoryStateStore, LocalNonceManager};
pub use public_mempool::PublicMempool;
pub use signers::LocalSigner;
pub use transport::{Transport, TransportBuildError, TransportBuilder, TransportConfig, Vendor};
