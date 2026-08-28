//! Concrete implementations of the [`core::deps`](crate::core::deps) ports. One module per
//! adapter; multi-file adapters ([`policy`], [`store`], [`submission`], [`transport`]) get a
//! subdirectory.

pub mod accounts;
pub mod clock;
pub mod ens;
pub mod gas_oracle;
pub mod nonce;
pub mod policy;
#[cfg(feature = "pricing")]
pub mod pricing;
pub mod read;
pub mod signers;
pub mod store;
pub mod submission;
pub mod transport;

/// Shared Multicall3 batching primitive used by the read adapter (and, later, preview).
pub(crate) mod multicall;

pub use accounts::AccountManager;
pub use clock::SystemClock;
pub use ens::RpcEnsResolver;
pub use gas_oracle::RpcGasOracle;
pub use nonce::LocalNonceManager;
#[cfg(feature = "pricing")]
pub use pricing::{ChainlinkPrice, FeedConfig, TokenListSource};
pub use read::RpcReadClient;
pub use signers::LocalSigner;
pub use store::InMemoryStateStore;
#[cfg(feature = "postgres")]
pub use store::PostgresStateStore;
#[cfg(feature = "redb")]
pub use store::RedbStateStore;
pub use submission::{PrivateMev, PublicMempool, Router};
pub use transport::{Transport, TransportBuildError, TransportBuilder, TransportConfig, Vendor};
