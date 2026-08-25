use crate::core::wallet::{Decision, PolicyOutcome, SigningRequest};
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

    /// Side-effect-free dry-run: **would** this request be allowed, and if not, why? The
    /// policy analog of [`Wallet::dry_run`](crate::Wallet::dry_run). Returns a
    /// [`PolicyOutcome`], which structurally cannot carry an approval — a preview can never
    /// become a signing path.
    ///
    /// The default routes through [`evaluate`](Self::evaluate) and drops the approval, which
    /// is safe only because approval minting is pure construction. An engine whose `evaluate`
    /// has real side effects (a remote call, a nonce reservation, a quorum request) MUST
    /// override this with a genuinely non-minting path.
    ///
    /// `validate` is **advisory**: policy state can change between it and `evaluate` (TOCTOU),
    /// so a passing dry-run must never short-circuit the real gate at sign time.
    async fn validate(&self, request: &SigningRequest) -> Result<PolicyOutcome, PolicyEngineError> {
        Ok(match self.evaluate(request).await? {
            Decision::Allow(_) => PolicyOutcome::WouldAllow,
            Decision::Deny(rejection) => PolicyOutcome::WouldDeny(rejection),
        })
    }
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
