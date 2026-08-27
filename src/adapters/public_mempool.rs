//! `PublicMempool` — the default [`SubmissionStrategy`]: a thin passthrough to
//! `eth_sendRawTransaction`. The seam lets private/relayer/paymaster strategies slot in
//! without touching the pipeline.

use crate::core::deps::{
    Rpc, SubmissionError, SubmissionOpts, SubmissionRoute, SubmissionStrategy,
};
use crate::obs::debug;
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;
use std::sync::Arc;

/// The default [`SubmissionStrategy`]: broadcast
/// straight to the public mempool via `eth_sendRawTransaction`.
pub struct PublicMempool {
    rpc: Arc<dyn Rpc>,
}

impl PublicMempool {
    /// Build over an RPC transport.
    pub fn new(rpc: Arc<dyn Rpc>) -> Self {
        Self { rpc }
    }
}

#[async_trait]
impl SubmissionStrategy for PublicMempool {
    async fn submit(
        &self,
        signed_rlp: Bytes,
        opts: &SubmissionOpts,
    ) -> Result<TxHash, SubmissionError> {
        // The Router sends only `Public` here; a `Private` route reaching the public mempool
        // would be a routing-invariant break, so broadcasting it publicly could leak a tx
        // meant to stay private. Assert in debug; in release, still broadcast (liveness).
        debug_assert!(
            matches!(opts.route, SubmissionRoute::Public),
            "PublicMempool received a non-public route: {:?}",
            opts.route
        );
        debug!("broadcasting signed transaction");
        Ok(self.rpc.send_raw(signed_rlp).await?)
    }
}
