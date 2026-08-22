use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;

/// Broadcasts a signed, RLP-encoded transaction and returns its hash. Phase 1 is
/// public-mempool only.
#[async_trait]
pub trait SubmissionStrategy: Send + Sync {
    async fn submit(&self, signed_rlp: Bytes) -> Result<TxHash, SubmissionError>;
}

/// Variants grow with the submission adapter (Task 15).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmissionError {}
