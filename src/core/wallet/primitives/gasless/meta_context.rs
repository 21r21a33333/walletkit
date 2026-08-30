//! [`MetaContext`] — the confirm-safety context stamped on a gasless outer `execute()` tx.
//! Its presence tells the executor to decode `ExecutedForwardRequest`: a mined outer tx whose
//! inner call reverted (`success = false`) settles `Failed`, never `Confirmed` — H's ethic
//! for meta-transactions.

use super::forward_request::ExecutedForwardRequest;
use crate::core::deps::TaskId;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::Log;
use alloy_sol_types::SolEvent;
use serde::{Deserialize, Serialize};

/// Non-secret context identifying the forwarder `execute()` behind a gasless send. Persisted on
/// the [`TxHandle`](crate::core::wallet::TxHandle) so the confirm path can decode the request's
/// `ExecutedForwardRequest(signer, nonce, success)` — the Gelato api key is never stored here.
///
/// Two shapes, distinguished by [`task`](Self::task):
/// - **self-relay** (`task = None`): we sent the outer `execute()` ourselves, so confirm-safety
///   is the OZ [`inner_succeeded`](Self::inner_succeeded) event decode over the receipt.
/// - **managed relay** (`task = Some`): a third party (Gelato) submits, so the executor first
///   polls the task to an on-chain hash; the relay's `ExecSuccess` verdict is the safety signal,
///   and the event decode does not apply (a different forwarder emits a different event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MetaContext {
    /// The forwarder that emitted the event (the EIP-712 `verifyingContract`).
    pub forwarder: Address,
    /// The user the request was signed by — the indexed `signer` in the event.
    pub signer: Address,
    /// The forwarder nonce the request consumed — disambiguates the matching event.
    pub nonce: U256,
    /// Present iff a managed relay submits this request: the task to poll until an on-chain hash
    /// appears. `None` for self-relay (the outer tx's hash is known at send time).
    #[serde(default)]
    pub task: Option<TaskId>,
}

impl MetaContext {
    /// The confirm-safety context for a signed request: which forwarder emitted the event, and
    /// the `signer`/`nonce` that identify this request's `ExecutedForwardRequest`.
    pub(crate) fn for_request(signed: &crate::core::deps::SignedRequest) -> Self {
        Self {
            forwarder: signed.forwarder,
            signer: signed.request.from,
            nonce: signed.request.nonce,
            task: None,
        }
    }

    /// The tracking context for a managed-relay (Gelato) send: the relay `verifyingContract` the
    /// user signed against, the `user`, the request's nonce (or `0` for salt-based concurrent
    /// sends), and the `task` the executor polls to an on-chain hash. Unlike self-relay, the
    /// on-chain event is not decoded — the relay's `ExecSuccess` verdict gates the recorded hash.
    pub(crate) fn for_gelato_task(
        forwarder: Address,
        user: Address,
        nonce: U256,
        task: TaskId,
    ) -> Self {
        Self {
            forwarder,
            signer: user,
            nonce,
            task: Some(task),
        }
    }

    /// Whether the forwarder actually executed the user's inner call. Decodes the
    /// `ExecutedForwardRequest` matching this request's `signer`+`nonce` and returns its
    /// `success`; a missing event counts as failure. **A succeeding outer tx is not enough** —
    /// the forwarder consumes the nonce and emits `success = false` when the inner call reverts.
    pub fn inner_succeeded(&self, logs: &[Log]) -> bool {
        logs.iter()
            .find_map(|log| {
                let event = ExecutedForwardRequest::decode_log_data(&log.inner.data).ok()?;
                (event.signer == self.signer && event.nonce == self.nonce).then_some(event.success)
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(signer: Address, nonce: U256, success: bool) -> Log {
        let data = ExecutedForwardRequest {
            signer,
            nonce,
            success,
        }
        .encode_log_data();
        Log {
            inner: alloy_primitives::Log {
                address: Address::ZERO,
                data,
            },
            ..Default::default()
        }
    }

    // The J invariant at the decode: a mined outer tx confirms only when the matching event
    // reports the inner call succeeded — anything else (reverted inner, absent or mismatched
    // event) is a failure, so the executor can never falsely `Confirm`.
    #[test]
    fn inner_succeeded_only_on_a_matching_success_event() {
        let ctx = MetaContext {
            forwarder: Address::ZERO,
            signer: Address::repeat_byte(1),
            nonce: U256::from(7u64),
            task: None,
        };

        assert!(ctx.inner_succeeded(&[log(ctx.signer, ctx.nonce, true)]));
        assert!(!ctx.inner_succeeded(&[log(ctx.signer, ctx.nonce, false)]));
        assert!(!ctx.inner_succeeded(&[]), "no event ⇒ not proven ⇒ false");
        assert!(
            !ctx.inner_succeeded(&[log(Address::repeat_byte(2), ctx.nonce, true)]),
            "different signer ⇒ not our request"
        );
        assert!(
            !ctx.inner_succeeded(&[log(ctx.signer, U256::from(8u64), true)]),
            "different nonce ⇒ not our request"
        );
    }
}
