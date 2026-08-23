//! `PublicMempool` — the Phase-1 [`SubmissionStrategy`]: a thin passthrough to
//! `eth_sendRawTransaction`. The seam lets Phase 2 add private/relayer/paymaster
//! strategies without touching the pipeline.

use crate::core::deps::{Rpc, SubmissionError, SubmissionStrategy};
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;
use std::sync::Arc;

pub struct PublicMempool {
    rpc: Arc<dyn Rpc>,
}

impl PublicMempool {
    pub fn new(rpc: Arc<dyn Rpc>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl SubmissionStrategy for PublicMempool {
    async fn submit(&self, signed_rlp: Bytes) -> Result<TxHash, SubmissionError> {
        Ok(self.rpc.send_raw(signed_rlp).await?)
    }
}
