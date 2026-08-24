//! Concrete implementations of the [`core::deps`](crate::core::deps) ports. One module per
//! adapter; multi-file adapters ([`policy`], [`store`], [`transport`]) get a subdirectory.

pub mod clock;
pub mod ens;
pub mod gas_oracle;
pub mod nonce;
pub mod policy;
#[cfg(feature = "pricing")]
pub mod pricing;
pub mod public_mempool;
pub mod read;
pub mod signers;
pub mod store;
pub mod transport;

/// Shared Multicall3 batching primitive used by the read adapter (and, later, preview).
pub(crate) mod multicall;

pub use clock::SystemClock;
pub use ens::RpcEnsResolver;
pub use gas_oracle::RpcGasOracle;
pub use nonce::LocalNonceManager;
#[cfg(feature = "pricing")]
pub use pricing::{ChainlinkPrice, FeedConfig, TokenListSource};
pub use public_mempool::PublicMempool;
pub use read::RpcReadClient;
pub use signers::LocalSigner;
pub use store::InMemoryStateStore;
#[cfg(feature = "postgres")]
pub use store::PostgresStateStore;
#[cfg(feature = "redb")]
pub use store::RedbStateStore;
pub use transport::{Transport, TransportBuildError, TransportBuilder, TransportConfig, Vendor};
