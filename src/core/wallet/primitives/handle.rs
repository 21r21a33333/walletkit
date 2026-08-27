use super::{GasEnvelope, IntentHash, TxIntent};
use alloy_primitives::{Address, B256, Bytes, TxHash, keccak256};
use serde::{Deserialize, Serialize};

/// Stable, queryable id for a tracked transaction — derived from intent + nonce so
/// it survives gas bumps (OZ `transactionId` / thirdweb `queueId` model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HandleId(B256);

impl HandleId {
    /// Derive the id from an intent hash and the nonce it occupies — stable across bumps
    /// (same intent + nonce) but distinct per resubmission at a new nonce.
    pub fn new(intent_hash: IntentHash, nonce: u64) -> Self {
        let mut buf = [0u8; 40];
        buf[..32].copy_from_slice(intent_hash.as_slice());
        buf[32..].copy_from_slice(&nonce.to_be_bytes());
        Self(keccak256(buf))
    }

    /// The raw 32-byte id — how durable stores key a handle. (Only durable backends key by
    /// the raw bytes; the cfg widens as each is added.)
    #[cfg(any(feature = "redb", feature = "postgres"))]
    pub(crate) fn as_bytes(self) -> [u8; 32] {
        self.0.0
    }
}

/// Lifecycle of a tracked transaction. Only `Confirmed`/`Failed`/`Replaced` are
/// terminal, and each is reached only at `required_confirmations` depth — so a reorg
/// before then re-tracks: `Mined`/`Replacing` fall back to `Sent`. This is the
/// depth-gated finality of OZ Defender (12 confs) / thirdweb / Alchemy; `Replaced`
/// on first sight would lose a tx whose nonce a reorg later frees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TxStatus {
    /// Accepted by the executor, not yet broadcast.
    Pending,
    /// Broadcast to the mempool, not yet observed in a block.
    Sent,
    /// Our tx is in a block; the outcome settles at depth. `block_hash` detects a reorg.
    Mined {
        /// Block number the tx landed in.
        block: u64,
        /// Hash of that block; a change under the same number signals a reorg.
        block_hash: B256,
    },
    /// Our nonce was consumed by a tx that isn't ours, but not yet `required` deep.
    /// `since_block` (head when first seen) is the depth clock; a reorg reverts to `Sent`.
    Replacing {
        /// Head height when the foreign tx was first seen — the depth clock for finality.
        since_block: u64,
    },
    /// Reached `required_confirmations` depth successfully. Terminal.
    Confirmed {
        /// Block number at which required depth was reached.
        block: u64,
    },
    /// Reverted or was rejected at depth. Terminal.
    Failed {
        /// Human-readable revert/failure reason.
        reason: String,
    },
    /// A *foreign* tx took this nonce to required depth. Terminal.
    Replaced,
    /// We cancelled this tx: a self-send at its nonce evicted it. Terminal, distinct from
    /// `Replaced` (a *foreign* tx taking the nonce).
    Dropped,
}

impl TxStatus {
    /// Terminal statuses will not change again, so the executor stops tracking them.
    /// Only depth-confirmed outcomes qualify — shallower states can still reorg.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Confirmed { .. } | Self::Failed { .. } | Self::Replaced | Self::Dropped
        )
    }
}

/// Stable, persisted handle to a submitted transaction — the single source of truth
/// for a tracked tx: the queryable unit a caller tracks, the crash-recovery WAL
/// record, and everything the executor needs to bump it, *except* the
/// [`PolicyApproval`](super::PolicyApproval) capability (which is never persisted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxHandle {
    /// Stable id, derived from `intent_hash` + `nonce`; survives gas bumps.
    pub id: HandleId,
    /// The account the transaction is sent from.
    pub account: Address,
    /// The semantic request, retained so a bump can re-evaluate policy (the signed bytes
    /// drop `purpose`).
    pub intent: TxIntent,
    /// Content hash of `intent` — the policy/tracking correlation key.
    pub intent_hash: IntentHash,
    /// The account nonce this transaction occupies.
    pub nonce: u64,
    /// Current lifecycle state.
    pub status: TxStatus,
    /// Immutable per-intent spend ceiling; a bump must never exceed it.
    pub envelope: GasEnvelope,
    /// Latest signed transaction bytes, persisted before broadcast so it can be
    /// rebroadcast verbatim; `fees`/`gas_limit` are decoded from it on demand.
    pub signed: Bytes,
    /// The original hash and each bump hash, so the mined hash can be told from a replacement.
    pub broadcasts: Vec<TxHash>,
    /// Unix seconds of the last broadcast; drives the bump timeout.
    pub last_broadcast_at: u64,
    /// Set by `cancel(id)`: its nonce being consumed settles the handle as `Dropped`.
    #[serde(default)]
    pub cancelled: bool,
}
