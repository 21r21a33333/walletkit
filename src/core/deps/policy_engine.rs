use crate::core::wallet::{Decision, TxIntent};
use async_trait::async_trait;

/// The pre-sign gate. Every engine (native / Regorus / WASM / remote) implements
/// this and returns a [`Decision`] — `Allow` with the host-minted approval, or
/// `Deny`. An operational failure (eval error, plugin trap, network) is `Err`,
/// which the caller treats **fail-closed** (never sign, maybe retry); a
/// `Decision::Deny` is a terminal denial. The host mints the approval, so no
/// engine — including third-party plugins — can forge authorization.
#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn evaluate(&self, intent: &TxIntent) -> Result<Decision, PolicyEngineError>;
}

/// Variants grow with the engines that produce them (Regorus/WASM/remote).
/// The native engine never errors — it returns `Ok(Decision::Deny)` instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyEngineError {}
