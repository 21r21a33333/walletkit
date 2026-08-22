use super::IntentHash;
use alloy_primitives::{B256, TxHash, keccak256};

/// Stable, queryable id for a tracked transaction — derived from intent + nonce so
/// it survives gas bumps (OZ `transactionId` / thirdweb `queueId` model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandleId(B256);

impl HandleId {
    pub fn new(intent_hash: IntentHash, nonce: u64) -> Self {
        let mut buf = [0u8; 40];
        buf[..32].copy_from_slice(intent_hash.as_slice());
        buf[32..].copy_from_slice(&nonce.to_be_bytes());
        Self(keccak256(buf))
    }
}

/// Lifecycle of a tracked transaction. Phase 1 produces `Pending` and `Sent`; the
/// executor (Task 17) adds the mined/confirmed/failed/replaced/dropped transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxStatus {
    Pending,
    Sent,
}

/// Stable, persisted handle to a submitted transaction — the queryable unit a
/// caller tracks. `broadcasts` holds the original and (Task 17) each bump, so the
/// mined hash distinguishes ours from a replacement.
#[derive(Debug, Clone)]
pub struct TxHandle {
    pub id: HandleId,
    pub intent_hash: IntentHash,
    pub nonce: u64,
    pub status: TxStatus,
    pub broadcasts: Vec<TxHash>,
}
