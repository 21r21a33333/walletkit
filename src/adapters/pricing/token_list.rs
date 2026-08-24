//! `TokenListSource` — token metadata from a Uniswap-schema token list parsed once into an
//! in-memory map (no RPC at read time), with an optional on-chain [`ReadClient`] fallback
//! for tokens the list omits.

use crate::core::deps::{Erc20Metadata, PricingError, ReadClient, ReadError, TokenMetadataSource};
use alloy_primitives::Address;
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

pub struct TokenListSource {
    tokens: HashMap<(u64, Address), Erc20Metadata>,
    fallback: Option<Arc<dyn ReadClient>>,
}

#[derive(Deserialize)]
struct TokenList {
    tokens: Vec<TokenListEntry>,
}

#[derive(Deserialize)]
struct TokenListEntry {
    #[serde(rename = "chainId")]
    chain_id: u64,
    address: Address,
    name: String,
    symbol: String,
    decimals: u8,
}

impl TokenListSource {
    /// Parse a Uniswap-schema token-list JSON into an in-memory map. Addresses are
    /// normalized by parsing into [`Address`], so lookups are case-insensitive.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PricingError> {
        let list: TokenList = serde_json::from_slice(bytes).map_err(|e| PricingError::List {
            detail: e.to_string(),
        })?;
        let tokens = list
            .tokens
            .into_iter()
            .map(|t| {
                (
                    (t.chain_id, t.address),
                    Erc20Metadata {
                        name: t.name,
                        symbol: t.symbol,
                        decimals: t.decimals,
                    },
                )
            })
            .collect();
        Ok(Self {
            tokens,
            fallback: None,
        })
    }

    /// Fill list misses from chain via a [`ReadClient`].
    pub fn with_fallback(mut self, read: Arc<dyn ReadClient>) -> Self {
        self.fallback = Some(read);
        self
    }

    /// Synchronous map lookup (no fallback) — the pure core the async path builds on.
    pub fn lookup(&self, chain_id: u64, token: Address) -> Option<&Erc20Metadata> {
        self.tokens.get(&(chain_id, token))
    }
}

#[async_trait]
impl TokenMetadataSource for TokenListSource {
    async fn metadata(
        &self,
        chain_id: u64,
        token: Address,
    ) -> Result<Option<Erc20Metadata>, PricingError> {
        if let Some(metadata) = self.lookup(chain_id, token) {
            return Ok(Some(metadata.clone()));
        }
        match &self.fallback {
            // A transport failure is a real error; a non-conforming token (decode failure)
            // simply has no usable metadata → `None`.
            Some(read) => match read.erc20_metadata(token).await {
                Ok(metadata) => Ok(Some(metadata)),
                Err(ReadError::Rpc(e)) => Err(PricingError::Rpc(e)),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uniswap_schema_and_looks_up_case_insensitively() {
        let json = br#"{"tokens":[
            {"chainId":1,"address":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","name":"USD Coin","symbol":"USDC","decimals":6}
        ]}"#;
        let src = TokenListSource::from_json(json).unwrap();

        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let md = src.lookup(1, usdc).unwrap();
        assert_eq!((md.symbol.as_str(), md.decimals), ("USDC", 6));

        // A lowercase address parses to the same key.
        let lower: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        assert!(src.lookup(1, lower).is_some());

        // Miss on unknown address / wrong chain.
        assert!(src.lookup(1, Address::ZERO).is_none());
        assert!(src.lookup(10, usdc).is_none());
    }
}
