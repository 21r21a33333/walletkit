//! Concrete implementations of the [`core::deps`](crate::core::deps) ports.
//! One (flat) module per adapter.

pub mod clock;
pub mod gas_oracle;
pub mod nonce_store;
pub mod policy;
#[cfg(feature = "postgres")]
pub mod postgres_store;
pub mod public_mempool;
#[cfg(feature = "redb")]
pub mod redb_store;
pub mod signers;
pub mod transport;

pub use clock::SystemClock;
pub use gas_oracle::RpcGasOracle;
pub use nonce_store::{InMemoryStateStore, LocalNonceManager};
#[cfg(feature = "postgres")]
pub use postgres_store::PostgresStateStore;
pub use public_mempool::PublicMempool;
#[cfg(feature = "redb")]
pub use redb_store::RedbStateStore;
pub use signers::LocalSigner;
pub use transport::{Transport, TransportBuildError, TransportBuilder, TransportConfig, Vendor};
