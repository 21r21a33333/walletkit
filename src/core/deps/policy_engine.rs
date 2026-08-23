use crate::core::wallet::{Decision, SigningRequest};
use async_trait::async_trait;

/// The pre-sign gate for every [`SigningRequest`] — a tx, an EIP-191 message, or EIP-712
/// typed data. Every engine (native / Regorus / WASM / remote) implements this and returns
/// a [`Decision`] — `Allow` with the host-minted approval, or `Deny`. An operational failure
/// (eval error, plugin trap, network) is `Err`, which the caller treats **fail-closed**
/// (never sign, maybe retry); a `Decision::Deny` is a terminal denial. The host mints the
/// approval, so no engine — including third-party plugins — can forge authorization.
#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn evaluate(&self, request: &SigningRequest) -> Result<Decision, PolicyEngineError>;
}

/// The native engine never errors (returns `Ok(Decision::Deny)`); these variants
/// come from engines that can fail operationally (WASM host, remote).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PolicyEngineError {
    /// The plugin could not be loaded at construction (bad module, hash mismatch,
    /// compile/instantiation failure).
    #[error("failed to load policy plugin: {0}")]
    Load(String),
    /// Evaluation failed operationally: a trap, a resource-limit/timeout, or a
    /// missing/malformed decision from the plugin.
    #[error("policy evaluation failed: {0}")]
    Eval(String),
}
