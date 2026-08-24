//! `StateStore` adapters: the in-memory default plus the durable backends (opt-in via
//! their features). One backend-agnostic conformance suite holds them to the same contract.

pub mod memory;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "redb")]
pub mod redb;

pub use memory::InMemoryStateStore;
#[cfg(feature = "postgres")]
pub use postgres::PostgresStateStore;
#[cfg(feature = "redb")]
pub use redb::RedbStateStore;
