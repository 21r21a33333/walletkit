//! `Router` — dispatches each submit to the public mempool or a private relay per
//! [`SubmissionOpts::route`]. Wired only when a private relay is configured; without one the
//! wallet uses [`PublicMempool`](super::PublicMempool) directly, so the public path is
//! unchanged and zero-cost.

use crate::core::deps::{SubmissionError, SubmissionOpts, SubmissionRoute, SubmissionStrategy};
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;
use std::sync::Arc;

/// Routes each submit to `public` or `private` by `opts.route`. Both arms are the same
/// [`SubmissionStrategy`] port, so the executor and pipeline stay route-agnostic.
pub struct Router {
    public: Arc<dyn SubmissionStrategy>,
    private: Arc<dyn SubmissionStrategy>,
}

impl Router {
    /// Build over the public and private strategies.
    pub fn new(public: Arc<dyn SubmissionStrategy>, private: Arc<dyn SubmissionStrategy>) -> Self {
        Self { public, private }
    }
}

#[async_trait]
impl SubmissionStrategy for Router {
    async fn submit(
        &self,
        signed_rlp: Bytes,
        opts: &SubmissionOpts,
    ) -> Result<TxHash, SubmissionError> {
        match &opts.route {
            SubmissionRoute::Public => self.public.submit(signed_rlp, opts).await,
            SubmissionRoute::Private(_) => self.private.submit(signed_rlp, opts).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::{Escalation, PrivateRoute, Relay};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct Recorder {
        hits: AtomicU32,
    }

    #[async_trait]
    impl SubmissionStrategy for Recorder {
        async fn submit(
            &self,
            _rlp: Bytes,
            _opts: &SubmissionOpts,
        ) -> Result<TxHash, SubmissionError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(TxHash::ZERO)
        }
    }

    #[tokio::test]
    async fn dispatches_by_route() {
        let public = Arc::new(Recorder::default());
        let private = Arc::new(Recorder::default());
        let router = Router::new(public.clone(), private.clone());

        router
            .submit(Bytes::new(), &SubmissionOpts::default())
            .await
            .unwrap();
        assert_eq!(public.hits.load(Ordering::SeqCst), 1);
        assert_eq!(private.hits.load(Ordering::SeqCst), 0);

        let opts = SubmissionOpts {
            route: SubmissionRoute::Private(PrivateRoute::new(
                Relay::MevBlocker,
                Escalation::StayPrivate,
            )),
        };
        router.submit(Bytes::new(), &opts).await.unwrap();
        assert_eq!(public.hits.load(Ordering::SeqCst), 1);
        assert_eq!(private.hits.load(Ordering::SeqCst), 1);
    }
}
