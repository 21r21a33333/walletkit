//! `Router` — the single submission strategy the pipeline holds. It dispatches each send to
//! the public mempool or, when configured, a private relay, per [`SubmissionOpts::route`].
//! Whether private routing exists lives here (an `Option`), not as a flag on the wallet.

use crate::core::deps::{
    RouteError, SubmissionError, SubmissionOpts, SubmissionRoute, SubmissionStrategy,
};
use alloy_primitives::{Bytes, TxHash};
use async_trait::async_trait;
use std::sync::Arc;

/// Routes each submit by `opts.route`. Both arms are the [`SubmissionStrategy`] port, so the
/// executor and pipeline stay route-agnostic. `private` is `None` until a relay identity is
/// configured; a `Private` send is then rejected up front via
/// [`SubmissionStrategy::supports_route`].
pub struct Router {
    public: Arc<dyn SubmissionStrategy>,
    private: Option<Arc<dyn SubmissionStrategy>>,
}

impl Router {
    /// Build over the public strategy and an optional private-relay strategy.
    pub fn new(
        public: Arc<dyn SubmissionStrategy>,
        private: Option<Arc<dyn SubmissionStrategy>>,
    ) -> Self {
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
            SubmissionRoute::Private(_) => match &self.private {
                Some(private) => private.submit(signed_rlp, opts).await,
                // The pipeline checks `supports_route` first, so this only guards the
                // recover/bump path if a private handle outlives its relay config.
                None => Err(RouteError::RelayNotConfigured.into()),
            },
        }
    }

    fn supports_route(&self, route: &SubmissionRoute) -> bool {
        match route {
            SubmissionRoute::Public => true,
            SubmissionRoute::Private(_) => self.private.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::deps::{Escalation, Protect};
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

    fn private_opts() -> SubmissionOpts {
        Protect::mev_blocker(Escalation::StayPrivate).into()
    }

    #[tokio::test]
    async fn dispatches_by_route() {
        let public = Arc::new(Recorder::default());
        let private = Arc::new(Recorder::default());
        let router = Router::new(public.clone(), Some(private.clone()));

        router
            .submit(Bytes::new(), &SubmissionOpts::public())
            .await
            .unwrap();
        assert_eq!(public.hits.load(Ordering::SeqCst), 1);
        assert_eq!(private.hits.load(Ordering::SeqCst), 0);

        router.submit(Bytes::new(), &private_opts()).await.unwrap();
        assert_eq!(public.hits.load(Ordering::SeqCst), 1);
        assert_eq!(private.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn without_a_private_arm_private_is_unsupported_and_rejected() {
        let public = Arc::new(Recorder::default());
        let router = Router::new(public, None);
        assert!(router.supports_route(&SubmissionRoute::Public));
        assert!(!router.supports_route(&private_opts().route));
        let err = router
            .submit(Bytes::new(), &private_opts())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SubmissionError::Route(RouteError::RelayNotConfigured)
        ));
    }
}
