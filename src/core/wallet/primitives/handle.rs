use super::{GasEnvelope, IntentHash, TxIntent};
use alloy_primitives::{Address, B256, Bytes, TxHash, keccak256};
use serde::{Deserialize, Serialize};

/// Stable, queryable id for a tracked transaction — derived from intent + nonce so
/// it survives gas bumps (OZ `transactionId` / thirdweb `queueId` model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HandleId(B256);

impl HandleId {
    pub fn new(intent_hash: IntentHash, nonce: u64) -> Self {
        let mut buf = [0u8; 40];
        buf[..32].copy_from_slice(intent_hash.as_slice());
        buf[32..].copy_from_slice(&nonce.to_be_bytes());
        Self(keccak256(buf))
    }
}

/// Lifecycle of a tracked transaction. Only `Confirmed`/`Failed`/`Replaced` are
/// terminal, and each is reached only at `required_confirmations` depth — so a reorg
/// before then re-tracks: `Mined`/`Replacing` fall back to `Sent`. This is the
/// depth-gated finality of OZ Defender (12 confs) / thirdweb / Alchemy; `Replaced`
/// on first sight would lose a tx whose nonce a reorg later frees.
/// (`Dropped` arrives with the Send phase that can produce it.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TxStatus {
    Pending,
    Sent,
    /// Our tx is in a block; the outcome settles at depth. `block_hash` detects a reorg.
    Mined {
        block: u64,
        block_hash: B256,
    },
    /// Our nonce was consumed by a tx that isn't ours, but not yet `required` deep.
    /// `since_block` (head when first seen) is the depth clock; a reorg reverts to `Sent`.
    Replacing {
        since_block: u64,
    },
    Confirmed {
        block: u64,
    },
    Failed {
        reason: String,
    },
    Replaced,
}

impl TxStatus {
    /// Terminal statuses will not change again, so the executor stops tracking them.
    /// Only depth-confirmed outcomes qualify — shallower states can still reorg.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Confirmed { .. } | Self::Failed { .. } | Self::Replaced
        )
    }
}

/// Stable, persisted handle to a submitted transaction — the single source of truth
/// for a tracked tx: the queryable unit a caller tracks, the crash-recovery WAL
/// record, and everything the executor needs to bump it, *except* the
/// [`PolicyApproval`](super::PolicyApproval) capability (which is never persisted).
///
/// `intent` is the semantic request (kept so a bump can re-evaluate policy — the
/// signed bytes drop `purpose`); `envelope` is the immutable per-intent spend ceiling
/// a bump must never exceed; `signed` is the latest signed tx (persisted before
/// broadcast so it can be rebroadcast verbatim), with `fees`/`gas_limit` decoded from
/// it on demand; `broadcasts` holds the original and each bump hash, so the mined hash
/// distinguishes ours from a replacement; `last_broadcast_at` drives the bump timeout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxHandle {
    pub id: HandleId,
    pub account: Address,
    pub intent: TxIntent,
    pub intent_hash: IntentHash,
    pub nonce: u64,
    pub status: TxStatus,
    pub envelope: GasEnvelope,
    pub signed: Bytes,
    pub broadcasts: Vec<TxHash>,
    pub last_broadcast_at: u64,
}
