//! [`SubmissionStrategy`] — the transaction-broadcast port (public mempool, private relay, …).

use crate::core::deps::RpcError;
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;

/// Broadcasts a signed, RLP-encoded transaction and returns its hash.
#[async_trait]
pub trait SubmissionStrategy: Send + Sync {
    /// Broadcast `signed_rlp` and return the transaction hash.
    async fn submit(&self, signed_rlp: Bytes) -> Result<TxHash, SubmissionError>;
}

/// Why a broadcast failed; its predicates classify the failure for the executor.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmissionError {
    /// The underlying RPC broadcast call failed.
    #[error(transparent)]
    Rpc(#[from] RpcError),
}

impl SubmissionError {
    /// Transient/indeterminate (network/timeout/5xx/rate-limit): the tx may already be
    /// in flight, so the caller may assume it was sent rather than reject it.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Rpc(RpcError::Call {
                transient: true,
                ..
            })
        )
    }

    /// The node already accepted this tx or its nonce ("already known" / "nonce too
    /// low"): it is effectively sent or mined, not rejected — keep the nonce and let
    /// the executor's confirm settle it.
    pub fn is_already_accepted(&self) -> bool {
        // JSON-RPC has no structured code for these, so match the canonical geth/reth
        // messages (case-insensitively).
        const ALREADY_ACCEPTED: [&str; 3] = ["already known", "already imported", "nonce too low"];
        match self {
            Self::Rpc(RpcError::Call { message, .. }) => {
                let message = message.to_ascii_lowercase();
                ALREADY_ACCEPTED.iter().any(|m| message.contains(m))
            }
        }
    }

    /// A replacement rejected as underpriced ("replacement transaction underpriced"): a
    /// competing tx at this nonce out-bids ours. Retryable — re-price higher and resend.
    pub fn is_underpriced(&self) -> bool {
        match self {
            Self::Rpc(RpcError::Call { message, .. }) => message
                .to_ascii_lowercase()
                .contains("replacement transaction underpriced"),
        }
    }
}
