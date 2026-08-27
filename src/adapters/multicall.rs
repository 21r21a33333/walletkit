//! `Multicall` — a small read-only builder over Multicall3 `aggregate3`: accumulate
//! `(target, calldata)` pairs with per-call failure isolation, execute in one (chunked)
//! round-trip, and get back a
//! `Vec<MulticallResult>` the caller decodes per its own type. Encapsulates the
//! Multicall3 ABI, the canonical address, and the node-cap chunking so callers batch
//! reads without touching any of it.

use crate::adapters::transport::rpc_err;
use crate::core::deps::RpcError;
use alloy_contract::Error as ContractError;
use alloy_primitives::{Address, Bytes, address};
use alloy_provider::DynProvider;
use alloy_sol_types::{SolCall, SolType, SolValue};

/// Canonical Multicall3 deployment — the same address on every chain (keyless deploy).
pub const MULTICALL3_ADDRESS: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

/// Max sub-calls per `aggregate3` round-trip; a larger batch is split across round-trips. A
/// conservative heuristic (not a protocol limit) to stay under a node's `eth_call` gas/calldata cap.
const MAX_CALLS_PER_BATCH: usize = 400;

alloy_sol_types::sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }
        function getEthBalance(address addr) external view returns (uint256);
        function aggregate3(Call3[] calls) external payable returns (Result[] returnData);
    }
}

/// One sub-call's outcome: a success flag plus the raw return bytes.
#[derive(Debug, Clone)]
pub struct MulticallResult {
    pub success: bool,
    pub return_data: Bytes,
}

impl MulticallResult {
    /// Decode a successful sub-call's return as an ABI value; `None` if the sub-call
    /// failed (reverted / non-conforming) or the data won't decode to `T`.
    pub fn decode<T>(&self) -> Option<T>
    where
        T: SolValue + From<<T::SolType as SolType>::RustType>,
    {
        self.success
            .then(|| T::abi_decode(&self.return_data).ok())
            .flatten()
    }
}

/// A batch of read-only calls executed through Multicall3 `aggregate3` (allow-failure per
/// call). Build with [`add`](Self::add) / [`add_eth_balance`](Self::add_eth_balance), then
/// [`call`](Self::call).
pub struct Multicall {
    provider: DynProvider,
    address: Address,
    calls: Vec<IMulticall3::Call3>,
}

impl Multicall {
    /// A batch over the canonical Multicall3 address.
    pub fn new(provider: DynProvider) -> Self {
        Self {
            provider,
            address: MULTICALL3_ADDRESS,
            calls: Vec::new(),
        }
    }

    /// Queue a typed call against `target`; a revert is isolated to this entry.
    pub fn add<C: SolCall>(&mut self, target: Address, call: &C) -> &mut Self {
        self.push(target, call.abi_encode())
    }

    /// Queue Multicall3's own `getEthBalance(account)` — folds a native-balance read into
    /// the batch so a wallet overview is a single round-trip.
    pub fn add_eth_balance(&mut self, account: Address) -> &mut Self {
        let data = IMulticall3::getEthBalanceCall { addr: account }.abi_encode();
        self.push(self.address, data)
    }

    fn push(&mut self, target: Address, call_data: Vec<u8>) -> &mut Self {
        self.calls.push(IMulticall3::Call3 {
            target,
            allowFailure: true,
            callData: Bytes::from(call_data),
        });
        self
    }

    /// Execute the batch, splitting into `aggregate3` round-trips under the node cap and
    /// concatenating results in call order.
    pub async fn call(&self) -> Result<Vec<MulticallResult>, RpcError> {
        let mc = IMulticall3::new(self.address, &self.provider);
        let mut out = Vec::with_capacity(self.calls.len());
        for chunk in self.calls.chunks(MAX_CALLS_PER_BATCH) {
            let results = mc
                .aggregate3(chunk.to_vec())
                .call()
                .await
                .map_err(contract_error)?;
            out.extend(results.into_iter().map(|r| MulticallResult {
                success: r.success,
                return_data: r.returnData,
            }));
        }
        Ok(out)
    }
}

/// An alloy contract-call error → our port error. A transport failure keeps its transient
/// classification; anything else (empty return = Multicall3 not deployed, or a decode
/// fault) is terminal. Shared with the single-read path in the read adapter.
pub(crate) fn contract_error(e: ContractError) -> RpcError {
    match e {
        ContractError::TransportError(te) => rpc_err(te),
        other => RpcError::Call {
            transient: false,
            message: other.to_string(),
        },
    }
}
