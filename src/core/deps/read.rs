//! `ReadClient` — object-safe, RPC-only chain reads for **known** contract addresses.
//! Token/NFT *discovery* ("which tokens does X hold") is an indexer concern and stays
//! out of scope. Only concrete domain types cross this port; `sol!` types are confined
//! to the adapter.

use crate::core::deps::RpcError;
use alloy_primitives::{Address, Bytes, U256};
use async_trait::async_trait;

#[async_trait]
pub trait ReadClient: Send + Sync {
    async fn chain_id(&self) -> Result<u64, ReadError>;
    /// `eth_getCode` at latest; [`is_contract`](Self::is_contract) is `!code.is_empty()`.
    async fn code(&self, address: Address) -> Result<Bytes, ReadError>;
    async fn is_contract(&self, address: Address) -> Result<bool, ReadError>;
    async fn native_balance(&self, account: Address) -> Result<U256, ReadError>;
    async fn erc20_balance(&self, token: Address, account: Address) -> Result<U256, ReadError>;
    async fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, ReadError>;
    async fn erc20_metadata(&self, token: Address) -> Result<Erc20Metadata, ReadError>;
    async fn erc721_owner_of(&self, token: Address, token_id: U256) -> Result<Address, ReadError>;
    async fn erc721_balance(&self, token: Address, account: Address) -> Result<U256, ReadError>;
    async fn erc1155_balance(
        &self,
        token: Address,
        account: Address,
        id: U256,
    ) -> Result<U256, ReadError>;
    /// A wallet overview in one Multicall3 `aggregate3`: the native balance folded in
    /// plus each token's `balanceOf`, per-token `Result` so a single reverting or
    /// non-conforming token can't fail the scan.
    async fn balances(
        &self,
        account: Address,
        tokens: &[Address],
    ) -> Result<AccountBalances, ReadError>;
}

/// ERC-20 display metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Erc20Metadata {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

/// A wallet overview: native balance plus one per-token result.
#[non_exhaustive]
pub struct AccountBalances {
    pub native: U256,
    pub tokens: Vec<TokenBalance>,
}

/// One token's balance in an [`AccountBalances`] batch; `Err` isolates a token whose
/// `balanceOf` reverted (non-conforming contract) without failing the whole read.
pub struct TokenBalance {
    pub token: Address,
    pub balance: Result<U256, ReadError>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// An on-chain response could not be decoded to the expected type (bad return data,
    /// or a sub-call that reverted inside a batch).
    #[error("failed to decode {context}")]
    Decode { context: &'static str },
}
