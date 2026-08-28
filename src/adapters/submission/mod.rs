//! Implementations of the [`SubmissionStrategy`](crate::core::deps::SubmissionStrategy) port
//! — the transaction-broadcast seam. [`Router`] is the one the pipeline holds; it dispatches
//! each send to [`PublicMempool`] or, when a relay identity is configured, [`PrivateMev`].

pub mod private_mev;
pub mod public_mempool;
pub mod router;

pub use private_mev::PrivateMev;
pub use public_mempool::PublicMempool;
pub use router::Router;
