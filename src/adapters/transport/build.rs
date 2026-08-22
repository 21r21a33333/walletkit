//! Construction for [`Transport`]: the [`TransportBuilder`] (in-process failover/
//! hedge/retry/timeout/auth-headers/throttle over a custom `reqwest` client) and
//! the declarative [`TransportConfig`].

use super::{Transport, rpc_err};
use crate::core::deps::RpcError;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_client::ClientBuilder;
use alloy_transport::layers::{FallbackLayer, RetryBackoffLayer, ThrottleLayer};
use alloy_transport_http::Http;
use alloy_transport_http::reqwest::Client;
use alloy_transport_http::reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use std::num::NonZeroUsize;
use std::time::Duration;
use tower::Layer;
use url::Url;

const DEFAULT_RETRY_MAX: u32 = 5;
const DEFAULT_RETRY_BACKOFF_MS: u64 = 200;
/// Compute-units-per-second budget for the retry policy (set high; we don't self-throttle).
const RETRY_CUPS: u64 = 100_000;
/// Throttle is always installed (avoids conditional layer types); this rps ≈ "off".
const UNLIMITED_RPS: u32 = 1_000_000;

impl Transport {
    /// Start a configurable HTTP transport at `primary`.
    pub fn builder(primary: Url) -> TransportBuilder {
        TransportBuilder::new(primary)
    }

    /// A single HTTP endpoint with defaults — the recommended eRPC setup.
    pub fn single(url: Url) -> Self {
        TransportBuilder::new(url).build()
    }

    /// Build from a declarative config (the first endpoint is primary, the rest
    /// are fallbacks). Panics if `endpoints` is empty.
    pub fn from_config(cfg: TransportConfig) -> Self {
        let mut endpoints = cfg.endpoints.into_iter();
        let primary = endpoints
            .next()
            .expect("TransportConfig needs at least one endpoint");
        let mut b = TransportBuilder::new(primary)
            .retry(cfg.retry_max, cfg.retry_backoff_ms)
            .hedge(cfg.hedge)
            .fallbacks(endpoints);
        if let Some(ms) = cfg.timeout_ms {
            b = b.timeout(Duration::from_millis(ms));
        }
        if let Some(rps) = cfg.rate_limit_rps {
            b = b.rate_limit(rps);
        }
        if let Some(token) = &cfg.bearer {
            b = b.bearer(token);
        }
        b.build()
    }

    /// Connect by URL scheme (http/ws/ipc) with defaults — WS/IPC use their native
    /// transport (reconnection etc.), skipping the HTTP layer stack. For rich HTTP
    /// config use [`Transport::builder`].
    pub async fn connect(url: &str) -> Result<Self, RpcError> {
        let client = ClientBuilder::default()
            .connect(url)
            .await
            .map_err(rpc_err)?;
        Ok(Self {
            provider: ProviderBuilder::new().connect_client(client).erased(),
        })
    }
}

/// Configurable HTTP transport builder — reuses alloy's `RetryBackoffLayer`,
/// `FallbackLayer`, and `ThrottleLayer` plus a custom `reqwest` client.
pub struct TransportBuilder {
    primary: Url,
    fallbacks: Vec<Url>,
    retry_max: u32,
    retry_backoff_ms: u64,
    hedge: usize,
    timeout: Option<Duration>,
    rate_limit_rps: Option<u32>,
    headers: HeaderMap,
}

impl TransportBuilder {
    fn new(primary: Url) -> Self {
        Self {
            primary,
            fallbacks: Vec::new(),
            retry_max: DEFAULT_RETRY_MAX,
            retry_backoff_ms: DEFAULT_RETRY_BACKOFF_MS,
            hedge: 1,
            timeout: None,
            rate_limit_rps: None,
            headers: HeaderMap::new(),
        }
    }

    /// Add a fallback endpoint (tried when the primary fails).
    pub fn fallback(mut self, url: Url) -> Self {
        self.fallbacks.push(url);
        self
    }

    pub fn fallbacks(mut self, urls: impl IntoIterator<Item = Url>) -> Self {
        self.fallbacks.extend(urls);
        self
    }

    pub fn retry(mut self, max_attempts: u32, backoff_ms: u64) -> Self {
        self.retry_max = max_attempts;
        self.retry_backoff_ms = backoff_ms;
        self
    }

    /// Query `n` endpoints in parallel and take the first success. `1` (default) is
    /// pure failover; `>1` hedges (faster, but broadcasts writes to multiple nodes).
    pub fn hedge(mut self, n: usize) -> Self {
        self.hedge = n.max(1);
        self
    }

    /// Per-request timeout (total).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Client-side rate limit in requests per second.
    pub fn rate_limit(mut self, requests_per_second: u32) -> Self {
        self.rate_limit_rps = Some(requests_per_second);
        self
    }

    /// A default header on every request (e.g. `x-api-key`). Invalid name/value pairs
    /// are ignored.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            self.headers.insert(n, v);
        }
        self
    }

    /// Set `Authorization: Bearer <token>` on every request.
    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub fn build(self) -> Transport {
        let mut client_builder = Client::builder().default_headers(self.headers);
        if let Some(t) = self.timeout {
            client_builder = client_builder.timeout(t);
        }
        let http_client = client_builder.build().expect("reqwest client");

        let retry = RetryBackoffLayer::new(self.retry_max, self.retry_backoff_ms, RETRY_CUPS);
        let throttle = ThrottleLayer::new(self.rate_limit_rps.unwrap_or(UNLIMITED_RPS));

        let client = if self.fallbacks.is_empty() {
            ClientBuilder::default()
                .layer(throttle)
                .layer(retry)
                .http_with_client(http_client, self.primary)
        } else {
            let transports: Vec<_> = std::iter::once(self.primary)
                .chain(self.fallbacks)
                .map(|u| Http::with_client(http_client.clone(), u))
                .collect();
            let active = NonZeroUsize::new(self.hedge.min(transports.len())).unwrap();
            let fallback = FallbackLayer::default()
                .with_active_transport_count(active)
                .layer(transports);
            ClientBuilder::default()
                .layer(throttle)
                .layer(retry)
                .transport(fallback, false)
        };

        Transport {
            provider: ProviderBuilder::new().connect_client(client).erased(),
        }
    }
}

/// Declarative per-chain transport config (deserialize from a config file).
#[derive(Debug, Clone, Deserialize)]
pub struct TransportConfig {
    /// The first endpoint is primary; the rest are fallbacks.
    pub endpoints: Vec<Url>,
    #[serde(default = "default_retry_max")]
    pub retry_max: u32,
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
    #[serde(default = "default_hedge")]
    pub hedge: usize,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub rate_limit_rps: Option<u32>,
    #[serde(default)]
    pub bearer: Option<String>,
}

fn default_retry_max() -> u32 {
    DEFAULT_RETRY_MAX
}
fn default_retry_backoff_ms() -> u64 {
    DEFAULT_RETRY_BACKOFF_MS
}
fn default_hedge() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke test: exercise every builder knob + the config path (guards the
    // FallbackLayer/NonZeroUsize/reqwest construction against panics). No network.
    #[test]
    fn builder_and_config_construct_without_panic() {
        let _ = Transport::builder("http://localhost:8545".parse().unwrap())
            .fallback("http://localhost:8546".parse().unwrap())
            .hedge(2)
            .retry(3, 100)
            .timeout(Duration::from_secs(10))
            .bearer("token")
            .rate_limit(50)
            .build();

        let cfg: TransportConfig = serde_json::from_str(
            r#"{"endpoints":["http://localhost:8545","http://localhost:8546"],"hedge":2,"timeout_ms":5000,"bearer":"k"}"#,
        )
        .unwrap();
        let _ = Transport::from_config(cfg);
    }
}
