//! Policy-engine adapters implementing the [`PolicyEngine`](crate::core::deps::PolicyEngine)
//! port. [`native`] is the zero-dependency default; [`moonpay`] implements MoonPay's
//! Open Wallet Standard (declarative rules + a sandboxed `executable`) behind a
//! feature flag, on the internal [`wasm`] plugin runner.

pub mod native;

#[cfg(feature = "policy-moonpay")]
pub mod moonpay;
#[cfg(feature = "policy-moonpay")]
mod wasm;

#[cfg(feature = "policy-moonpay")]
pub use moonpay::MoonPayPolicyEngine;
pub use native::{DefaultPolicyEngine, Policy, SpendLimit, TargetAllowlist, Verdict};
