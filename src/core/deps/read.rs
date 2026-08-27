//! `ReadClient` — object-safe, RPC-only chain reads for **known** contract addresses.
//! Token/NFT *discovery* ("which tokens does X hold") is an indexer concern and stays
//! out of scope. Only concrete domain types cross this port; `sol!` types are confined
//! to the adapter.

use crate::core::deps::RpcError;
use alloy_primitives::{Address, Bytes, U256};
use async_trait::async_trait;

/// RPC-only reads for known contract addresses: native/ERC-20/721/1155 balances,
/// ERC-20 metadata and allowance, code checks, and a Multicall3-batched overview.
#[async_trait]
pub trait ReadClient: Send + Sync {
    /// The connected chain's id (`eth_chainId`).
    async fn chain_id(&self) -> Result<u64, ReadError>;
    /// `eth_getCode` at latest; [`is_contract`](Self::is_contract) is `!code.is_empty()`.
    async fn code(&self, address: Address) -> Result<Bytes, ReadError>;
    /// Whether `address` has deployed code (i.e. is a contract, not an EOA).
    async fn is_contract(&self, address: Address) -> Result<bool, ReadError>;
    /// Native-token balance (wei) of `account`.
    async fn native_balance(&self, account: Address) -> Result<U256, ReadError>;
    /// ERC-20 `balanceOf(account)` for `token`.
    async fn erc20_balance(&self, token: Address, account: Address) -> Result<U256, ReadError>;
    /// ERC-20 `allowance(owner, spender)` for `token`.
    async fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, ReadError>;
    /// ERC-20 name/symbol/decimals for `token`.
    async fn erc20_metadata(&self, token: Address) -> Result<Erc20Metadata, ReadError>;
    /// ERC-721 `ownerOf(token_id)` for `token`.
    async fn erc721_owner_of(&self, token: Address, token_id: U256) -> Result<Address, ReadError>;
    /// ERC-721 `balanceOf(account)` (NFT count) for `token`.
    async fn erc721_balance(&self, token: Address, account: Address) -> Result<U256, ReadError>;
    /// ERC-1155 `balanceOf(account, id)` for `token`.
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
    /// Token name (`name()`).
    pub name: String,
    /// Token symbol (`symbol()`).
    pub symbol: String,
    /// Decimal places (`decimals()`).
    pub decimals: u8,
}

/// A wallet overview: native balance plus one per-token result.
#[non_exhaustive]
pub struct AccountBalances {
    /// Native-token balance (wei).
    pub native: U256,
    /// Per-token balances, aligned with the requested token list.
    pub tokens: Vec<TokenBalance>,
}

/// One token's balance in an [`AccountBalances`] batch; `Err` isolates a token whose
/// `balanceOf` reverted (non-conforming contract) without failing the whole read.
pub struct TokenBalance {
    /// The token contract address.
    pub token: Address,
    /// Its `balanceOf`, or the per-token error that isolated it.
    pub balance: Result<U256, ReadError>,
}

/// Why a chain read failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    /// The underlying RPC call failed.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// An on-chain response could not be decoded to the expected type (bad return data,
    /// or a sub-call that reverted inside a batch).
    #[error("failed to decode {context}")]
    Decode {
        /// What was being decoded when it failed.
        context: &'static str,
    },
}
