//! Policy-engine adapters implementing the [`PolicyEngine`](crate::core::deps::PolicyEngine)
//! port. [`native`] is the zero-dependency default; Regorus (8b) and WASM (8c)
//! plug in behind feature flags as siblings here.

pub mod native;

pub use native::{DefaultPolicyEngine, Policy, SpendLimit, TargetAllowlist, Verdict};
