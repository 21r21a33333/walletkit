//! Account management: HD derivation schemes, derived-account records, and
//! counterfactual smart-account address prediction. Zero I/O; the seed-owning
//! factory lives in `adapters::accounts`.

mod primitives;
pub use primitives::*;
