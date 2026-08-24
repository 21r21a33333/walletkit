//! `RpcReadClient` — the one [`ReadClient`] adapter, over a resilient alloy `DynProvider`
//! (the same one [`Transport`](crate::adapters::Transport) builds, so reads inherit its
//! failover/retry/hedge). Single reads use `sol!` contract instances; `balances` and
//! `erc20_metadata` batch through the `Multicall` builder. Only concrete domain types
//! cross the port — `sol!` types stay here.

use crate::adapters::multicall::{Multicall, MulticallResult, contract_error};
use crate::adapters::transport::rpc_err;
use crate::core::deps::{AccountBalances, Erc20Metadata, ReadClient, ReadError, TokenBalance};
use alloy_contract::Error as ContractError;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_transport::TransportError;
use async_trait::async_trait;

alloy_sol_types::sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function decimals() external view returns (uint8);
        function name() external view returns (string);
        function symbol() external view returns (string);
    }
    #[sol(rpc)]
    interface IERC721 {
        function ownerOf(uint256 tokenId) external view returns (address);
        function balanceOf(address owner) external view returns (uint256);
    }
    #[sol(rpc)]
    interface IERC1155 {
        function balanceOf(address account, uint256 id) external view returns (uint256);
    }
}

pub struct RpcReadClient {
    provider: DynProvider,
}

impl RpcReadClient {
    /// Build over a resilient provider — obtain one from
    /// [`Transport::provider`](crate::adapters::Transport::provider).
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }

    fn multicall(&self) -> Multicall {
        Multicall::new(self.provider.clone())
    }
}

#[async_trait]
impl ReadClient for RpcReadClient {
    async fn chain_id(&self) -> Result<u64, ReadError> {
        self.provider.get_chain_id().await.map_err(read_transport)
    }

    async fn code(&self, address: Address) -> Result<Bytes, ReadError> {
        self.provider
            .get_code_at(address)
            .await
            .map_err(read_transport)
    }

    async fn is_contract(&self, address: Address) -> Result<bool, ReadError> {
        Ok(!self.code(address).await?.is_empty())
    }

    async fn native_balance(&self, account: Address) -> Result<U256, ReadError> {
        self.provider
            .get_balance(account)
            .await
            .map_err(read_transport)
    }

    async fn erc20_balance(&self, token: Address, account: Address) -> Result<U256, ReadError> {
        IERC20::new(token, &self.provider)
            .balanceOf(account)
            .call()
            .await
            .map_err(read_contract)
    }

    async fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, ReadError> {
        IERC20::new(token, &self.provider)
            .allowance(owner, spender)
            .call()
            .await
            .map_err(read_contract)
    }

    async fn erc20_metadata(&self, token: Address) -> Result<Erc20Metadata, ReadError> {
        // name/symbol/decimals in one aggregate3 (three calls, one RPC).
        let mut mc = self.multicall();
        mc.add(token, &IERC20::nameCall {})
            .add(token, &IERC20::symbolCall {})
            .add(token, &IERC20::decimalsCall {});
        let results = mc.call().await?;
        let [name, symbol, decimals] = results.as_slice() else {
            return Err(ReadError::Decode {
                context: "erc20 metadata",
            });
        };
        Ok(Erc20Metadata {
            name: decode_metadata_string(name, "erc20 name")?,
            symbol: decode_metadata_string(symbol, "erc20 symbol")?,
            // `decimals()` is `uint8`; decode the word then narrow (a value >255 is a
            // non-conforming token, not a real decimals).
            decimals: decimals
                .decode::<U256>()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or(ReadError::Decode {
                    context: "erc20 decimals",
                })?,
        })
    }

    async fn erc721_owner_of(&self, token: Address, token_id: U256) -> Result<Address, ReadError> {
        IERC721::new(token, &self.provider)
            .ownerOf(token_id)
            .call()
            .await
            .map_err(read_contract)
    }

    async fn erc721_balance(&self, token: Address, account: Address) -> Result<U256, ReadError> {
        IERC721::new(token, &self.provider)
            .balanceOf(account)
            .call()
            .await
            .map_err(read_contract)
    }

    async fn erc1155_balance(
        &self,
        token: Address,
        account: Address,
        id: U256,
    ) -> Result<U256, ReadError> {
        IERC1155::new(token, &self.provider)
            .balanceOf(account, id)
            .call()
            .await
            .map_err(read_contract)
    }

    async fn balances(
        &self,
        account: Address,
        tokens: &[Address],
    ) -> Result<AccountBalances, ReadError> {
        // Native folds into the batch via Multicall3.getEthBalance; the builder chunks the
        // token balanceOf calls under the node cap. Per-call `Result` isolates a reverting
        // or non-conforming token so one bad entry can't fail the scan.
        let mut mc = self.multicall();
        mc.add_eth_balance(account);
        for &token in tokens {
            mc.add(token, &IERC20::balanceOfCall { owner: account });
        }
        let results = mc.call().await?;
        let Some((native, token_results)) = results.split_first() else {
            return Err(ReadError::Decode {
                context: "empty multicall",
            });
        };
        if token_results.len() != tokens.len() {
            return Err(ReadError::Decode {
                context: "multicall result length",
            });
        }
        let native = native.decode::<U256>().ok_or(ReadError::Decode {
            context: "native balance",
        })?;
        let tokens = tokens
            .iter()
            .zip(token_results)
            .map(|(&token, res)| TokenBalance {
                token,
                balance: res.decode::<U256>().ok_or(ReadError::Decode {
                    context: "erc20 balance",
                }),
            })
            .collect();
        Ok(AccountBalances { native, tokens })
    }
}

/// Decode an ERC-20 `name`/`symbol`, tolerating tokens that return `bytes32` instead of
/// `string` (MKR/DAI/SAI): try the ABI `string`, then fall back to a null-trimmed UTF-8
/// read of the raw 32 bytes (the Solady `MetadataReaderLib` / Uniswap `SafeERC20Namer` idiom).
fn decode_metadata_string(
    res: &MulticallResult,
    context: &'static str,
) -> Result<String, ReadError> {
    if !res.success {
        return Err(ReadError::Decode { context });
    }
    if let Some(s) = res.decode::<String>() {
        return Ok(s);
    }
    let bytes32: Vec<u8> = res
        .return_data
        .iter()
        .copied()
        .take(32)
        .take_while(|b| *b != 0)
        .collect();
    if bytes32.is_empty() {
        return Err(ReadError::Decode { context });
    }
    Ok(String::from_utf8_lossy(&bytes32).into_owned())
}

/// A raw provider read failed at the transport layer.
fn read_transport(e: TransportError) -> ReadError {
    ReadError::Rpc(rpc_err(e))
}

/// A `sol!` contract read failed (transport, empty return, or decode).
fn read_contract(e: ContractError) -> ReadError {
    ReadError::Rpc(contract_error(e))
}
