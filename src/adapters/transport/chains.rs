//! Zero-config RPC resolution — public defaults per chain and vendor-key URL
//! builders, so a transport "just works" without an explicit URL (overridable).
//!
//! ⚠️ **Public defaults are dev/getting-started only** — they are shared, rate-
//! limited, and carry no SLA. For production, pass your own URL to
//! [`Transport::builder`], use a vendor key via [`Transport::vendor`], or run eRPC.
//!
//! Public data is **eRPC's curated public-endpoints catalog**
//! (`https://evm-public-endpoints.erpc.cloud`, ~1000 chains, hourly-refreshed). The
//! crate embeds a snapshot (`public_endpoints.json`) for offline use;
//! [`refresh_public_endpoints`] pulls the live list at runtime (opt-in).

use super::{Transport, TransportBuildError, TransportBuilder};
use crate::core::deps::RpcError;
use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;
use url::Url;

/// eRPC's live, hourly-refreshed public-endpoints catalog (the "repository" source).
const EVM_PUBLIC_ENDPOINTS_URL: &str = "https://evm-public-endpoints.erpc.cloud";
/// Offline snapshot of the above, embedded in the binary.
const EMBEDDED: &str = include_str!("public_endpoints.json");
/// Cap on default endpoints per chain (primary + fallbacks) — avoids a huge fallback set.
const MAX_DEFAULT_ENDPOINTS: usize = 5;

fn registry() -> &'static RwLock<HashMap<u64, Vec<Url>>> {
    static REGISTRY: OnceLock<RwLock<HashMap<u64, Vec<Url>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(parse(EMBEDDED)))
}

/// Parse the eRPC catalog (`{"metadata":…, "<chainId>":{"endpoints":[…]}}`).
fn parse(json: &str) -> HashMap<u64, Vec<Url>> {
    #[derive(Deserialize)]
    struct Entry {
        endpoints: Vec<String>,
    }
    let raw: HashMap<String, serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    raw.into_iter()
        .filter_map(|(k, v)| {
            let id: u64 = k.parse().ok()?; // skips the "metadata" key
            let entry: Entry = serde_json::from_value(v).ok()?;
            Some((
                id,
                entry
                    .endpoints
                    .iter()
                    .filter_map(|u| Url::parse(u).ok())
                    .collect(),
            ))
        })
        .collect()
}

/// The curated public default endpoints for `chain_id` (empty if unknown).
pub fn public_rpcs(chain_id: u64) -> Vec<Url> {
    registry()
        .read()
        .get(&chain_id)
        .cloned()
        .unwrap_or_default()
}

/// Refresh the public-RPC registry from eRPC's live catalog (opt-in — the crate
/// ships an embedded snapshot; call this, or loop it, to stay current). Returns the
/// number of chains loaded. Leaves the existing registry intact on failure.
pub async fn refresh_public_endpoints() -> Result<usize, RpcError> {
    let net = |e: alloy_transport_http::reqwest::Error| RpcError::Call {
        transient: true,
        message: format!("refresh public endpoints: {e}"),
    };
    let body = alloy_transport_http::reqwest::Client::new()
        .get(EVM_PUBLIC_ENDPOINTS_URL)
        .send()
        .await
        .map_err(net)?
        .error_for_status()
        .map_err(net)?
        .text()
        .await
        .map_err(net)?;
    let fresh = parse(&body);
    if fresh.is_empty() {
        return Err(RpcError::Call {
            transient: true,
            message: "public endpoints response had no chains".into(),
        });
    }
    let count = fresh.len();
    *registry().write() = fresh;
    Ok(count)
}

/// A managed-RPC vendor whose endpoint is built from an API key + chain. Slug maps
/// and templates are lifted from eRPC's vendor implementations; `Thirdweb` is
/// chain-id-based, so it covers any chain.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Vendor {
    /// Alchemy.
    Alchemy,
    /// Infura.
    Infura,
    /// dRPC.
    Drpc,
    /// Ankr.
    Ankr,
    /// Tenderly.
    Tenderly,
    /// thirdweb (chain-id-based; covers any chain).
    Thirdweb,
}

/// The vendor's per-chain network slug (curated; `None` if unsupported).
fn vendor_slug(vendor: Vendor, chain_id: u64) -> Option<&'static str> {
    let map: &[(u64, &str)] = match vendor {
        Vendor::Alchemy => &[
            (1, "eth-mainnet"),
            (11155111, "eth-sepolia"),
            (8453, "base-mainnet"),
            (84532, "base-sepolia"),
            (10, "opt-mainnet"),
            (11155420, "opt-sepolia"),
            (42161, "arb-mainnet"),
            (421614, "arb-sepolia"),
            (137, "polygon-mainnet"),
        ],
        Vendor::Infura => &[
            (1, "mainnet"),
            (11155111, "sepolia"),
            (17000, "holesky"),
            (8453, "base-mainnet"),
            (84532, "base-sepolia"),
            (10, "optimism-mainnet"),
            (11155420, "optimism-sepolia"),
            (42161, "arbitrum-mainnet"),
            (421614, "arbitrum-sepolia"),
            (137, "polygon-mainnet"),
        ],
        Vendor::Drpc => &[
            (1, "ethereum"),
            (11155111, "sepolia"),
            (8453, "base"),
            (10, "optimism"),
            (42161, "arbitrum"),
            (137, "polygon"),
            (56, "bsc"),
            (100, "gnosis"),
        ],
        Vendor::Ankr => &[
            (1, "eth"),
            (11155111, "eth_sepolia"),
            (17000, "eth_holesky"),
            (8453, "base"),
            (84532, "base_sepolia"),
            (10, "optimism"),
            (11155420, "optimism_sepolia"),
            (42161, "arbitrum"),
            (421614, "arbitrum_sepolia"),
            (137, "polygon"),
            (56, "bsc"),
            (43114, "avalanche"),
            (100, "gnosis"),
        ],
        Vendor::Tenderly => &[
            (1, "mainnet"),
            (11155111, "sepolia"),
            (17000, "holesky"),
            (8453, "base"),
            (84532, "base-sepolia"),
            (10, "optimism"),
            (11155420, "optimism-sepolia"),
            (42161, "arbitrum"),
            (421614, "arbitrum-sepolia"),
            (137, "polygon"),
        ],
        // chain-id-based (handled in `vendor_url`, no slug map)
        Vendor::Thirdweb => &[],
    };
    map.iter()
        .find(|(id, _)| *id == chain_id)
        .map(|(_, slug)| *slug)
}

/// Build a vendor RPC URL from an API key + chain (`None` if the pair isn't mapped).
pub fn vendor_url(vendor: Vendor, api_key: &str, chain_id: u64) -> Option<Url> {
    // Chain-id-based vendors need no per-chain slug map — they cover any chain.
    if let Vendor::Thirdweb = vendor {
        return Url::parse(&format!("https://{chain_id}.rpc.thirdweb.com/{api_key}")).ok();
    }
    let slug = vendor_slug(vendor, chain_id)?;
    let raw = match vendor {
        Vendor::Alchemy => format!("https://{slug}.g.alchemy.com/v2/{api_key}"),
        Vendor::Infura => format!("https://{slug}.infura.io/v3/{api_key}"),
        Vendor::Drpc => format!("https://lb.drpc.org/ogrpc?network={slug}&dkey={api_key}"),
        Vendor::Ankr => format!("https://rpc.ankr.com/{slug}/{api_key}"),
        Vendor::Tenderly => format!("https://{slug}.gateway.tenderly.co/{api_key}"),
        Vendor::Thirdweb => unreachable!("handled above"),
    };
    Url::parse(&raw).ok()
}

impl Transport {
    /// A transport over a chain's curated **public** defaults (primary + up to a few
    /// fallbacks). Dev only — see the module docs. `Err` if the chain is unknown or the
    /// client can't be built.
    pub fn for_chain(chain_id: u64) -> Result<Transport, TransportBuildError> {
        Transport::builder_for_chain(chain_id)
            .ok_or(TransportBuildError::UnknownChain(chain_id))?
            .build()
    }

    /// A prefilled builder over a chain's public defaults, so you can still layer
    /// timeout/headers/rate-limit before building. `None` if the chain is unknown.
    pub fn builder_for_chain(chain_id: u64) -> Option<TransportBuilder> {
        let mut rpcs = public_rpcs(chain_id)
            .into_iter()
            .take(MAX_DEFAULT_ENDPOINTS);
        Some(Transport::builder(rpcs.next()?).fallbacks(rpcs))
    }

    /// A transport at a vendor endpoint built from an API key. `Err(UnknownChain)` if
    /// the `(vendor, chain)` pair isn't in the curated map (use [`Transport::builder`]).
    pub fn vendor(
        vendor: Vendor,
        api_key: &str,
        chain_id: u64,
    ) -> Result<Transport, TransportBuildError> {
        let url = vendor_url(vendor, api_key, chain_id)
            .ok_or(TransportBuildError::UnknownChain(chain_id))?;
        Transport::url(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_has_broad_coverage() {
        assert!(
            registry().read().len() > 100,
            "eRPC snapshot covers ~1000 chains"
        );
        for chain in [1, 10, 56, 137, 8453, 42161, 11155111] {
            assert!(
                !public_rpcs(chain).is_empty(),
                "chain {chain} has public defaults"
            );
        }
        assert!(public_rpcs(u64::MAX).is_empty(), "unknown chain has none");
        assert!(Transport::for_chain(8453).is_ok());
        assert!(matches!(
            Transport::for_chain(u64::MAX),
            Err(TransportBuildError::UnknownChain(_))
        ));
    }

    #[test]
    fn vendor_url_builds_expected_endpoints() {
        assert_eq!(
            vendor_url(Vendor::Alchemy, "KEY", 1).unwrap().as_str(),
            "https://eth-mainnet.g.alchemy.com/v2/KEY"
        );
        assert_eq!(
            vendor_url(Vendor::Infura, "KEY", 8453).unwrap().as_str(),
            "https://base-mainnet.infura.io/v3/KEY"
        );
        assert_eq!(
            vendor_url(Vendor::Drpc, "KEY", 10).unwrap().as_str(),
            "https://lb.drpc.org/ogrpc?network=optimism&dkey=KEY"
        );
        assert_eq!(
            vendor_url(Vendor::Ankr, "KEY", 42161).unwrap().as_str(),
            "https://rpc.ankr.com/arbitrum/KEY"
        );
        // Thirdweb is chain-id-based → works for any chain, even one with no slug.
        assert_eq!(
            vendor_url(Vendor::Thirdweb, "CID", 7777777)
                .unwrap()
                .as_str(),
            "https://7777777.rpc.thirdweb.com/CID"
        );
        assert!(vendor_url(Vendor::Alchemy, "KEY", 999_999).is_none());
    }
}
