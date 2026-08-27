//! `RpcEnsResolver` — ENS over `alloy-ens`'s `ProviderEnsExt` (registry → resolver).
//! Reverse lookups are **forward-verified** in-crate (a reverse record is user-settable and
//! unauthenticated, so a claimed name must resolve back to the same address), and not-found
//! is normalized to `Ok(None)`. CCIP-Read is not followed — offchain/L2 names surface as
//! `EnsError::OffchainLookupRequired`.

use crate::adapters::transport::rpc_err;
use crate::core::deps::{EnsError, EnsResolver};
use alloy_contract::Error as ContractError;
use alloy_ens::{EnsError as AlloyEnsError, ProviderEnsExt};
use alloy_primitives::Address;
use alloy_provider::DynProvider;
use async_trait::async_trait;

/// EIP-3668 `OffchainLookup(address,string[],bytes,bytes4,bytes)` error selector.
const OFFCHAIN_LOOKUP: [u8; 4] = [0x55, 0x6f, 0x18, 0x30];

/// The [`EnsResolver`] over a plain alloy provider
/// (forward-verified reverse lookups; strict RPC, no CCIP-Read gateway hop).
pub struct RpcEnsResolver {
    provider: DynProvider,
}

impl RpcEnsResolver {
    /// Build over a resilient provider — obtain one from
    /// [`Transport::provider`](crate::adapters::Transport::provider).
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl EnsResolver for RpcEnsResolver {
    async fn resolve_name(&self, name: &str) -> Result<Option<Address>, EnsError> {
        match self.provider.resolve_name(name).await {
            Ok(addr) if !addr.is_zero() => Ok(Some(addr)),
            Ok(_) => Ok(None),
            Err(e) => none_or_err(e),
        }
    }

    async fn reverse_lookup(&self, address: Address) -> Result<Option<String>, EnsError> {
        let name = match self.provider.lookup_address(&address).await {
            Ok(name) => name,
            Err(e) => return none_or_err(e),
        };
        // Forward-verify: the reverse record is only trustworthy if the name resolves back
        // to the queried address. `alloy-ens` does not do this.
        match self.provider.resolve_name(&name).await {
            Ok(forward) if forward == address => Ok(Some(name)),
            Ok(_) => Ok(None),
            Err(e) => none_or_err(e),
        }
    }

    async fn text_record(&self, name: &str, key: &str) -> Result<Option<String>, EnsError> {
        match self.provider.lookup_txt(name, key).await {
            Ok(value) if value.is_empty() => Ok(None),
            Ok(value) => Ok(Some(value)),
            Err(e) => none_or_err(e),
        }
    }
}

/// Map an alloy-ens error to our result. The registry "no resolver / no reverse registrar"
/// and any *resolution revert* (the Universal Resolver reverts `ResolverNotFound`,
/// `UnsupportedResolverProfile`, reverse mismatch, … for a name it can't resolve) are a
/// legitimate empty result; an `OffchainLookup` revert is the typed offchain error; a
/// transport failure (no revert data) keeps its transient classification via `RpcError`.
fn none_or_err<T>(e: AlloyEnsError) -> Result<Option<T>, EnsError> {
    if matches!(
        &e,
        AlloyEnsError::ResolverNotFound(_) | AlloyEnsError::ReverseRegistrarNotFound
    ) {
        return Ok(None);
    }
    let contract = match e {
        AlloyEnsError::Resolver(c)
        | AlloyEnsError::RevRegistrar(c)
        | AlloyEnsError::Lookup(c)
        | AlloyEnsError::Resolve(c)
        | AlloyEnsError::ResolveTxtRecord(c) => c,
        other => {
            return Err(EnsError::Resolution {
                detail: other.to_string(),
            });
        }
    };
    match revert_selector(&contract) {
        // The Universal Resolver signals an offchain (CCIP-Read) name by reverting here.
        Some(OFFCHAIN_LOOKUP) => Err(EnsError::OffchainLookupRequired),
        // Any other revert means the name can't be resolved on-chain → an empty result.
        Some(_) => Ok(None),
        // No revert data → a transport/node failure (keep it retryable) or a decode error.
        None => match contract {
            ContractError::TransportError(te) => Err(EnsError::Rpc(rpc_err(te))),
            other => Err(EnsError::Resolution {
                detail: other.to_string(),
            }),
        },
    }
}

/// The 4-byte revert selector of a contract call error, if it reverted with data.
fn revert_selector(e: &ContractError) -> Option<[u8; 4]> {
    let ContractError::TransportError(te) = e else {
        return None;
    };
    let data = te.as_error_resp()?.as_revert_data()?;
    data.get(..4)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_resolver_and_reverse_registrar_map_to_none() {
        // A name with no resolver / an address with no reverse record is an empty result,
        // not an error — the load-bearing distinction for callers.
        assert!(matches!(
            none_or_err::<Address>(AlloyEnsError::ResolverNotFound("x.eth".into())),
            Ok(None)
        ));
        assert!(matches!(
            none_or_err::<String>(AlloyEnsError::ReverseRegistrarNotFound),
            Ok(None)
        ));
    }
}
