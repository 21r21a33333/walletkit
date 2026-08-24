//! Concrete implementations of the [`core::deps`](crate::core::deps) ports. One module per
//! adapter; multi-file adapters ([`policy`], [`store`], [`transport`]) get a subdirectory.

pub mod clock;
pub mod gas_oracle;
pub mod nonce;
pub mod policy;
pub mod public_mempool;
pub mod signers;
pub mod store;
pub mod transport;

pub use clock::SystemClock;
pub use gas_oracle::RpcGasOracle;
pub use nonce::LocalNonceManager;
pub use public_mempool::PublicMempool;
pub use signers::LocalSigner;
pub use store::InMemoryStateStore;
#[cfg(feature = "postgres")]
pub use store::PostgresStateStore;
#[cfg(feature = "redb")]
pub use store::RedbStateStore;
pub use transport::{Transport, TransportBuildError, TransportBuilder, TransportConfig, Vendor};
