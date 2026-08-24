# Sub-project F1 — Read & Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **This repo is review-gated (CLAUDE.md).** The final step of every task is *not* an auto-commit: run the full gate (`cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`), report the **real** output, leave the changes **uncommitted**, and STOP for review. Commit only on explicit approval, then start the next task. The `git commit` shown in each task's last step is the command to run **after** approval.

**Goal:** Give consumers an RPC-only, object-safe **read** view of chain state (`ReadClient`) and a pre-sign **simulation** (`Wallet::dry_run` → `TxPreview`), plus ENS name resolution and an opt-in enrichment seam — without touching the sign/executor path.

**Architecture:** Three small object-safe read ports (`ReadClient`, `EnsResolver`, and the enrichment pair) + concrete adapters over the **same resilient alloy provider `Transport` already builds** (failover/retry/hedge inherited for free). Preview extends the existing `Rpc` port with `call`/`create_access_list` and lives in `core/wallet/preview.rs`. Reads are **standalone** (they target arbitrary addresses, not the wallet's one account); `Wallet::dry_run` is the only wallet-bound convenience. Everything here is read-only: no signer, no `PolicyApproval`, no state mutation.

**Tech Stack:** Rust 2024, `alloy-*` 2.4.1 line, `alloy-sol-types` 1.6.1 (`sol!`, `Revert`/`Panic` decode), **new deps** `alloy-contract` 2.4.1 (`#[sol(rpc)]` instances + `.call()`), `alloy-ens` 2.4.1 (`ProviderEnsExt`, `namehash`). Multicall3 via alloy's canonical `IMulticall3` at `0xcA11bde05977b3631167028862bE2a173976CA11` (predeployed by anvil ≥1.0). `async-trait`, `thiserror`, `serde`/`serde_json`, the `crate::obs` shim.

## Global Constraints

Every task's requirements implicitly include these (from CLAUDE.md + the approved spec `docs/plans/f-read-preview/2026-08-24-read-preview-design.md`):

- **Ports:** `core/deps/<port>.rs`, one file per port, object-safe (`#[async_trait]`, `Send + Sync`, used behind `Arc<dyn _>`), each with its own `thiserror` enum `{TraitName}Error`, `#[non_exhaustive]`. Never `Result<T, String>`. Adapters in `adapters/`. `sol!` types stay **inside** the adapter — only concrete domain types cross a port boundary.
- **One public error type:** any fallible method surfaced on `Wallet`/the facade returns `WalletKitError`; per-port `{Trait}Error`s map in via `From` and are classified in `kind()` (`Retryable`/`Terminal`/`NeedsReconcile`). A **revert in a preview is not an error** — it is a `TxPreview` with a `Revert` outcome.
- **Reuse before hand-rolling:** `sol!` typed ABI (no hand-rolled encoding), alloy-contract `.call()` (no bespoke request building), alloy's `IMulticall3` `aggregate3` (per-call `Result`), `alloy-ens` for namehash/resolution, `alloy_sol_types::{Revert, Panic, decode_revert_reason}` for revert decode. No `unwrap()`/`expect()` in production code (tests only).
- **Named returns, not positional tuples.** Multi-value returns are named structs.
- **Observability:** import `use crate::obs::{...}` and call bare; spans via `#[cfg_attr(feature = "tracing", tracing::instrument(...))]`. Build stays green **with and without** `--no-default-features`.
- **Comments = why-not-what**, short, minimal; no dev-process breadcrumbs (no task/phase numbers, no history narration, no future-promises). The pre-commit hook (`.githooks/pre-commit`) enforces this.
- **Tests earn their place:** no serde/struct-init/trait-plumbing tests; test only logic that can regress (multicall failure-isolation, revert decode, reverse-verify, price scale/staleness, token-list fallback). Adapter integration tests run against embedded anvil and **skip cleanly** when anvil is absent.
- **Locked design decisions (approved spec):** core is RPC-only and vendor-free; Multicall3 `aggregate3` per-call `Result` is the batch default with native balance folded in (one RPC); `TxPreview` is RPC-only and gas is advisory; enrichment ships only its RPC-compatible adapters behind feature `enrich`; reads are standalone; `ReadClient`/`EnsResolver` reuse `Transport`'s resilient provider.

---

## File map

| File | Responsibility | Task |
| --- | --- | --- |
| `Cargo.toml` | add `alloy-contract`, `alloy-sol-types`, `alloy-ens`; feature `enrich` | 1, 5 |
| `tests/support/mod.rs` | `deploy_mock_erc20` + `send_tx` helpers; `MockErc20` `sol!` | 1 |
| `tests/support/mock_erc20.bin` | committed creation bytecode fixture | 1 |
| `src/core/deps/read.rs` | `ReadClient` port + `ReadError`, `Erc20Metadata`, `AccountBalances` | 1 |
| `src/adapters/read.rs` | `RpcReadClient` (sol! `IERC20`/`IERC721`/`IERC1155`/`IMulticall3`) | 1 |
| `src/adapters/transport/mod.rs` | `Transport::provider()`; impl `Rpc::call`/`create_access_list` | 1, 2 |
| `src/core/deps/rpc.rs` | `Rpc::call`→`Simulated`, `Rpc::create_access_list`; `Simulated` enum | 2 |
| `src/core/wallet/preview.rs` | `TxPreview`, `SimOutcome`, `RevertReason`, `dry_run` orchestration | 3 |
| `src/facade.rs` | `Wallet::dry_run` | 3 |
| `src/core/deps/ens.rs` | `EnsResolver` port + `EnsError` | 4 |
| `src/adapters/ens.rs` | `RpcEnsResolver` over `alloy-ens` `ProviderEnsExt` | 4 |
| `src/core/deps/enrich.rs` | `TokenMetadataSource`, `PriceSource` ports + `EnrichError`, `Price`, `Currency` | 5 |
| `src/adapters/enrich/mod.rs`, `token_list.rs`, `chainlink.rs` | enrichment adapters (feature `enrich`) | 5 |
| `src/error.rs` | `WalletKitError::{Read, Ens}` + `From` + `kind()` | 1, 4 |
| `src/core/deps/mod.rs`, `src/adapters/mod.rs`, `src/lib.rs`, `src/core/wallet/mod.rs` | module wiring + re-exports | 1–5 |

---

### Task 1: `ReadClient` port + `RpcReadClient` adapter (with anvil mock-ERC20 test fixture)

**Files:**
- Modify: `Cargo.toml` (add `alloy-contract`, `alloy-sol-types` deps)
- Create: `src/core/deps/read.rs`
- Create: `src/adapters/read.rs`
- Modify: `src/core/deps/mod.rs` (add `pub mod read;` + re-exports)
- Modify: `src/adapters/mod.rs` (add `pub mod read;` + `pub use read::RpcReadClient;`)
- Modify: `src/adapters/transport/mod.rs` (add `provider()` accessor)
- Modify: `src/error.rs` (`WalletKitError::Read` + `From<ReadError>` + `kind()`)
- Create: `tests/support/mock_erc20.bin` (committed bytecode fixture)
- Modify: `tests/support/mod.rs` (`MockErc20` sol! + `deploy_mock_erc20`/`send_tx` helpers)
- Create: `tests/read.rs` (integration scenarios)

**Interfaces:**
- Consumes: `Transport` (from adapters), `alloy_provider::DynProvider`.
- Produces:
  - `trait ReadClient: Send + Sync` with `chain_id`, `code`, `is_contract`, `native_balance`, `erc20_balance`, `erc20_allowance`, `erc20_metadata`, `erc721_owner_of`, `erc721_balance`, `erc1155_balance`, `balances`. Returned structs are `#[non_exhaustive]`.
  - `struct Erc20Metadata { name: String, symbol: String, decimals: u8 }`
  - `struct AccountBalances { native: U256, tokens: Vec<TokenBalance> }`, `struct TokenBalance { token: Address, balance: Result<U256, ReadError> }`
  - `enum ReadError` (`#[non_exhaustive]`): `Rpc(RpcError)`, `Decode { context: &'static str }`.
  - `RpcReadClient::new(provider: DynProvider) -> Self`
  - `Transport::provider(&self) -> DynProvider`
  - Test helpers: `Localnet::deploy_mock_erc20(&self, deployer_index: u32) -> Address`, `Localnet::send_tx(&self, from_index: u32, to: Address, input: Bytes)`.

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` `[dependencies]`, add (keep alphabetical among the `alloy-*` block):

```toml
alloy-contract = "2.4.1"
alloy-sol-types = "1"
```

`alloy-sol-types` is already in the tree transitively (1.6.1); naming it directly is required because the adapter uses `sol!`, `SolCall`, `SolValue` directly. `alloy-contract` provides `#[sol(rpc)]` contract instances and `.call()`.

- [ ] **Step 2: Commit the mock-ERC20 bytecode fixture**

The read/preview integration tests need a deployed ERC-20. Create `tests/support/mock_erc20.bin` with this exact creation bytecode (compiled with `solc 0.8.30 --optimize` from a no-constructor-arg mock that mints `1_000_000e18` to the deployer; selectors match standard ERC-20: `balanceOf`=`0x70a08231`, `allowance`=`0xdd62ed3e`, `decimals`=`0x313ce567`, `name`=`0x06fdde03`, `symbol`=`0x95d89b41`, plus `revertWith`=`0xee2a10c4` which `revert("nope")`):

```
0x60c060405260046080908152634d6f636b60e01b60a0525f906100229082610110565b506040805180820190915260048152634d4f434b60e01b602082015260019061004b9082610110565b50348015610057575f5ffd5b50335f90815260026020526040902069d3c21bcecceda100000090556101ca565b634e487b7160e01b5f52604160045260245ffd5b600181811c908216806100a057607f821691505b6020821081036100be57634e487b7160e01b5f52602260045260245ffd5b50919050565b601f82111561010b57805f5260205f20601f840160051c810160208510156100e95750805b601f840160051c820191505b81811015610108575f81556001016100f5565b50505b505050565b81516001600160401b0381111561012957610129610078565b61013d81610137845461008c565b846100c4565b6020601f82116001811461016f575f83156101585750848201515b5f19600385901b1c1916600184901b178455610108565b5f84815260208120601f198516915b8281101561019e578785015182556020948501946001909201910161017e565b50848210156101bb57868401515f19600387901b60f8161c191681555b50505050600190811b01905550565b61048e806101d75f395ff3fe608060405234801561000f575f5ffd5b5060043610610090575f3560e01c806395d89b411161006357806395d89b411461011c578063a9059cbb14610124578063c50497ae14610137578063dd62ed3e14610148578063ee2a10c414610172575f5ffd5b806306fdde0314610094578063095ea7b3146100b2578063313ce567146100d557806370a08231146100ef575b5f5ffd5b61009c61017c565b6040516100a9919061031d565b60405180910390f35b6100c56100c036600461036d565b610207565b60405190151581526020016100a9565b6100dd601281565b60405160ff90911681526020016100a9565b61010e6100fd366004610395565b60026020525f908152604090205481565b6040519081526020016100a9565b61009c610235565b6100c561013236600461036d565b610242565b61010e69d3c21bcecceda100000081565b61010e6101563660046103b5565b600360209081525f928352604080842090915290825290205481565b61017a6102ec565b005b5f8054610188906103e6565b80601f01602080910402602001604051908101604052809291908181526020018280546101b4906103e6565b80156101ff5780601f106101d6576101008083540402835291602001916101ff565b820191905f5260205f20905b8154815290600101906020018083116101e257829003601f168201915b505050505081565b335f9081526003602090815260408083206001600160a01b0386168452909152902081905560015b92915050565b60018054610188906103e6565b335f908152600260205260408120548211156102945760405162461bcd60e51b815260206004820152600c60248201526b1a5b9cdd59999a58da595b9d60a21b60448201526064016040518091039060fd5b335f90815260026020526040812080548492906102b2908490610432565b90915550506001600160a01b0383165f90815260026020526040812080548492906102de908490610445565b909155506001949350505050565b60405162461bcd60e51b815260040161028b906020808252600490820152636e6f706560e01b604082015260600190565b602081525f82518060208401528060208501604085015e5f604082850101526040601f19601f83011684010191505092915050565b80356001600160a01b0381168114610368575f5ffd5b919050565b5f5f6040838503121561037e575f5ffd5b61038783610352565b946020939093013593505050565b5f602082840312156103a5575f5ffd5b6103ae82610352565b9392505050565b5f5f604083850312156103c6575f5ffd5b6103cf83610352565b91506103dd60208401610352565b90509250929050565b600181811c908216806103fa57607f821691505b60208210810361041857634e487b7160e01b5f52602260045260245ffd5b50919050565b634e487b7160e01b5f52601160045260245ffd5b8181038181111561022f5761022f61041e565b8082018082111561022f5761022f61041e56fea264697066735822122010a9634c52db0554cd97db829d12e79723ddaac2bbae17036111b2ed8841bc6064736f6c634300081e0033
```

> Provenance note for the reviewer (put in the PR description, not in a code comment): regenerate with `solc 0.8.30 --optimize --bin` on a minimal `MockErc20.sol` (name "Mock"/symbol "MOCK"/18 decimals, constructor mints `1_000_000 ether` to `msg.sender`, plus `approve`/`transfer`/`revertWith`). The `.bin` is committed so tests need no solc at build time.

- [ ] **Step 3: Add the harness deploy/send helpers + `provider()` accessor**

In `src/adapters/transport/mod.rs`, add to `impl Transport` a cheap provider clone (`DynProvider` is `Arc`-backed, clone is cheap; reads ride the same failover/retry layers):

```rust
impl Transport {
    /// A clone of the resilient provider this transport wraps, for read-only
    /// adapters (`RpcReadClient`, ENS) that need typed `sol!`/multicall access
    /// yet must inherit the same failover/retry/hedge as the write path.
    pub fn provider(&self) -> DynProvider {
        self.provider.clone()
    }
}
```

In `tests/support/mod.rs`, add a `sol!` mock binding and two helpers (the harness already holds a raw `DynProvider` in `self.provider` and sends external txs via `self.provider.send_transaction`). Use `alloy_sol_types::sol` and the funded anvil accounts:

```rust
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::sol;

sol! {
    #[sol(rpc)]
    interface MockErc20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function revertWith() external pure;
    }
}

impl Localnet {
    /// Deploy the committed mock ERC-20 from a funded anvil account and return its
    /// address. The constructor mints the full supply to the deployer.
    pub async fn deploy_mock_erc20(&self, deployer_index: u32) -> Address {
        let bin = include_str!("mock_erc20.bin").trim();
        let code = Bytes::from(alloy_primitives::hex::decode(bin).expect("valid hex"));
        let from = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, deployer_index)
            .expect("mnemonic")
            .address();
        let tx = alloy_rpc_types_eth::TransactionRequest::default()
            .from(from)
            .with_deploy_code(code);
        let receipt = self
            .provider
            .send_transaction(tx)
            .await
            .expect("deploy send")
            .get_receipt()
            .await
            .expect("deploy receipt");
        receipt.contract_address.expect("contract address")
    }

    /// Send a raw contract call (no value) from a funded account and wait for it to mine.
    pub async fn send_tx(&self, from_index: u32, to: Address, input: Bytes) {
        let from = LocalSigner::from_mnemonic(ANVIL_MNEMONIC, from_index)
            .expect("mnemonic")
            .address();
        let tx = alloy_rpc_types_eth::TransactionRequest::default()
            .from(from)
            .to(to)
            .input(input.into());
        self.provider
            .send_transaction(tx)
            .await
            .expect("send")
            .get_receipt()
            .await
            .expect("receipt");
    }
}
```

> Note: `TransactionRequest::with_deploy_code`, `.from`, `.to`, `.input` are alloy builder methods; anvil signs with its unlocked dev accounts, so the harness sends unsigned requests through the raw provider. If `with_deploy_code` differs in this alloy point release, set `to: None` + `input: TransactionInput::new(code)` explicitly (same effect).

- [ ] **Step 4: Write the `ReadClient` port**

Create `src/core/deps/read.rs`:

```rust
use crate::core::deps::RpcError;
use alloy_primitives::{Address, Bytes, U256};
use async_trait::async_trait;

/// Read-only chain queries for **known** contract addresses (discovery/enumeration
/// is an indexer concern, out of scope). RPC-only; object-safe.
#[async_trait]
pub trait ReadClient: Send + Sync {
    async fn chain_id(&self) -> Result<u64, ReadError>;
    /// `eth_getCode` at latest; `is_contract` is `!code.is_empty()` — EOA-vs-contract
    /// branching and the substrate for later EIP-1271-vs-ECDSA signature dispatch.
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
    /// One Multicall3 `aggregate3`: the native balance plus each token's `balanceOf`,
    /// with per-token `Result` so a single reverting token can't fail the overview.
    async fn balances(
        &self,
        account: Address,
        tokens: &[Address],
    ) -> Result<AccountBalances, ReadError>;
}

// `#[non_exhaustive]` on the returned structs so fields (a metadata `source`/`logo_uri`,
// a price `confidence`) can be added later without a breaking change — grow on demand (YAGNI).
/// ERC-20 display metadata.
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
    /// An on-chain response could not be decoded to the expected type.
    #[error("failed to decode {context}")]
    Decode { context: &'static str },
}
```

Wire it: in `src/core/deps/mod.rs` add `pub mod read;` and `pub use read::{AccountBalances, Erc20Metadata, ReadClient, ReadError, TokenBalance};`.

> `ReadError` must be `Clone`? No — `TokenBalance.balance: Result<U256, ReadError>` holds it by value, no clone needed. But the adapter constructs a fresh `ReadError` per failed token; `thiserror` + `RpcError` are not `Clone`, and we don't clone them — fine.

- [ ] **Step 5: Write the `RpcReadClient` adapter**

Create `src/adapters/read.rs`. `sol!` the interfaces (types stay inside the adapter), reuse `alloy-contract` instances for single reads and alloy's canonical `IMulticall3::aggregate3` for the batch:

```rust
//! `RpcReadClient` — the one [`ReadClient`] adapter, over a resilient alloy
//! `DynProvider` (the same one `Transport` builds). Typed reads use `sol!`
//! bindings; `balances` folds the native balance into a single Multicall3
//! `aggregate3` so one reverting token can't fail a portfolio scan.

use crate::core::deps::{
    AccountBalances, Erc20Metadata, ReadClient, ReadError, TokenBalance,
};
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_provider::DynProvider;
use alloy_sol_types::{SolCall, SolValue};
use async_trait::async_trait;

/// Canonical Multicall3 deployment (same on every chain; anvil predeploys it).
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");

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
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }
        function getEthBalance(address addr) external view returns (uint256);
        function aggregate3(Call3[] calls) external payable returns (Result[] returnData);
    }
}

pub struct RpcReadClient {
    provider: DynProvider,
}

impl RpcReadClient {
    /// Build over a resilient provider — obtain one from [`Transport::provider`].
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }

    /// One `aggregate3` round-trip; returns each call's raw result (success flag +
    /// return bytes) so the caller decodes per its own type.
    async fn aggregate3(
        &self,
        calls: Vec<IMulticall3::Call3>,
    ) -> Result<Vec<IMulticall3::Result>, ReadError> {
        let mc = IMulticall3::new(MULTICALL3, &self.provider);
        Ok(mc.aggregate3(calls).call().await.map_err(map_contract_err)?)
    }
}

/// A `Call3` that tolerates a revert (per-call failure isolation).
fn call3(target: Address, data: Vec<u8>) -> IMulticall3::Call3 {
    IMulticall3::Call3 { target, allowFailure: true, callData: Bytes::from(data) }
}

/// Multicall3 sub-calls per `aggregate3` — bounds calldata/gas so a large token list
/// can't exceed a node's `eth_call` cap; `balances` chunks and stitches.
const MAX_CALLS_PER_AGGREGATE: usize = 400;

#[async_trait]
impl ReadClient for RpcReadClient {
    async fn chain_id(&self) -> Result<u64, ReadError> {
        Ok(self.provider.get_chain_id().await.map_err(ReadError::from_rpc)?)
    }

    async fn code(&self, address: Address) -> Result<Bytes, ReadError> {
        Ok(self.provider.get_code_at(address).await.map_err(ReadError::from_rpc)?)
    }

    async fn is_contract(&self, address: Address) -> Result<bool, ReadError> {
        Ok(!self.code(address).await?.is_empty())
    }

    async fn native_balance(&self, account: Address) -> Result<U256, ReadError> {
        Ok(self.provider.get_balance(account).await.map_err(ReadError::from_rpc)?)
    }

    async fn erc20_balance(&self, token: Address, account: Address) -> Result<U256, ReadError> {
        let c = IERC20::new(token, &self.provider);
        Ok(c.balanceOf(account).call().await.map_err(map_contract_err)?)
    }

    async fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, ReadError> {
        let c = IERC20::new(token, &self.provider);
        Ok(c.allowance(owner, spender).call().await.map_err(map_contract_err)?)
    }

    async fn erc20_metadata(&self, token: Address) -> Result<Erc20Metadata, ReadError> {
        // name/symbol/decimals in one aggregate3 (three calls, one RPC).
        let calls = vec![
            call3(token, IERC20::nameCall {}.abi_encode()),
            call3(token, IERC20::symbolCall {}.abi_encode()),
            call3(token, IERC20::decimalsCall {}.abi_encode()),
        ];
        let r = self.aggregate3(calls).await?;
        // name/symbol may be `bytes32` on non-standard tokens (MKR/DAI/SAI) — try string, then bytes32.
        let name = decode_str(&r[0], "erc20 name")?;
        let symbol = decode_str(&r[1], "erc20 symbol")?;
        let decimals = decode_ret::<IERC20::decimalsCall>(&r[2], "erc20 decimals")?;
        Ok(Erc20Metadata { name, symbol, decimals })
    }

    async fn erc721_owner_of(&self, token: Address, token_id: U256) -> Result<Address, ReadError> {
        let c = IERC721::new(token, &self.provider);
        Ok(c.ownerOf(token_id).call().await.map_err(map_contract_err)?)
    }

    async fn erc721_balance(&self, token: Address, account: Address) -> Result<U256, ReadError> {
        let c = IERC721::new(token, &self.provider);
        Ok(c.balanceOf(account).call().await.map_err(map_contract_err)?)
    }

    async fn erc1155_balance(
        &self,
        token: Address,
        account: Address,
        id: U256,
    ) -> Result<U256, ReadError> {
        let c = IERC1155::new(token, &self.provider);
        Ok(c.balanceOf(account, id).call().await.map_err(map_contract_err)?)
    }

    async fn balances(
        &self,
        account: Address,
        tokens: &[Address],
    ) -> Result<AccountBalances, ReadError> {
        // Native folds into the first aggregate (Multicall3.getEthBalance); token balanceOf
        // calls are chunked so a large list can't exceed the node's eth_call gas/calldata cap.
        // At least one aggregate runs even for an empty token list (to read native).
        let raw = tokens.chunks(MAX_CALLS_PER_AGGREGATE - 1);
        let chunks: Vec<&[Address]> = if tokens.is_empty() { vec![&[]] } else { raw.collect() };
        let mut token_balances = Vec::with_capacity(tokens.len());
        let mut native = None;
        for (i, chunk) in chunks.iter().enumerate() {
            let mut calls = Vec::with_capacity(chunk.len() + 1);
            if i == 0 {
                calls.push(call3(MULTICALL3, IMulticall3::getEthBalanceCall { addr: account }.abi_encode()));
            }
            calls.extend(
                chunk.iter().map(|&t| call3(t, IERC20::balanceOfCall { owner: account }.abi_encode())),
            );
            let results = self.aggregate3(calls).await?;
            if results.len() != chunk.len() + usize::from(i == 0) {
                return Err(ReadError::Decode { context: "multicall result length" });
            }
            let mut results = results.into_iter();
            if i == 0 {
                let Some(n) = results.next() else {
                    return Err(ReadError::Decode { context: "native balance" });
                };
                native = Some(decode_ret::<IMulticall3::getEthBalanceCall>(&n, "native balance")?);
            }
            for (&token, res) in chunk.iter().zip(results) {
                token_balances.push(TokenBalance {
                    token,
                    balance: decode_ret::<IERC20::balanceOfCall>(&res, "erc20 balance"),
                });
            }
        }
        let Some(native) = native else {
            return Err(ReadError::Decode { context: "native balance" });
        };
        Ok(AccountBalances { native, tokens: token_balances })
    }
}

/// Decode one `aggregate3` result: a failed sub-call (`success == false`, e.g. a
/// non-conforming token) or a return that won't decode surfaces as `ReadError::Decode`.
fn decode_ret<C: SolCall>(res: &IMulticall3::Result, context: &'static str) -> Result<C::Return, ReadError> {
    if !res.success {
        return Err(ReadError::Decode { context });
    }
    C::abi_decode_returns(&res.returnData).map_err(|_| ReadError::Decode { context })
}

/// Decode an ERC-20 `name`/`symbol`, tolerating tokens that return `bytes32` instead of
/// `string` (MKR/DAI/SAI): try the ABI `string`, then fall back to a null-trimmed UTF-8
/// read of the raw 32 bytes (the Solady `MetadataReaderLib` / Uniswap `SafeERC20Namer` idiom).
fn decode_str(res: &IMulticall3::Result, context: &'static str) -> Result<String, ReadError> {
    if !res.success {
        return Err(ReadError::Decode { context });
    }
    if let Ok(s) = <String as SolValue>::abi_decode(&res.returnData) {
        return Ok(s);
    }
    let bytes32: Vec<u8> = res.returnData.iter().copied().take(32).take_while(|b| *b != 0).collect();
    if bytes32.is_empty() {
        return Err(ReadError::Decode { context });
    }
    Ok(String::from_utf8_lossy(&bytes32).into_owned())
}

/// alloy-contract call error → `ReadError`. A revert/decoding issue is terminal-ish
/// data; a transport failure keeps its transient classification via `RpcError`.
fn map_contract_err(e: alloy_contract::Error) -> ReadError {
    match e {
        alloy_contract::Error::TransportError(te) => ReadError::Rpc(rpc_from_transport(te)),
        _ => ReadError::Decode { context: "contract call" },
    }
}
```

Two glue helpers you must add (reuse the existing transport mapping — do **not** duplicate its logic):
1. In `src/adapters/transport/mod.rs`, make the existing `fn rpc_err(e: TransportError) -> RpcError` `pub(crate)` and re-export a thin `pub(crate) fn rpc_from_transport` alias, OR move `rpc_err` to a shared `adapters/transport/mod.rs` path the read adapter imports. Simplest: `use crate::adapters::transport::rpc_err as rpc_from_transport;` after changing `rpc_err`'s visibility to `pub(crate)`.
2. `ReadError::from_rpc`: add `impl ReadError { fn from_rpc(e: alloy_transport::TransportError) -> Self { Self::Rpc(rpc_from_transport(e)) } }` in the adapter module (not the port) — keep alloy types out of `core`. (`provider.get_balance` returns `alloy` `TransportError`.)

Wire it: `src/adapters/mod.rs` gets `pub mod read;` + `pub use read::RpcReadClient;`.

> `SolValue::abi_decode` is available if you ever need a bare `U256` decode; here `abi_decode_returns` on the `SolCall` types covers every case. `#[sol(rpc)]` needs `alloy-contract` in scope — the generated `IERC20::new` returns an `alloy_contract::CallBuilder`.

- [ ] **Step 6: Add the public error mapping**

In `src/error.rs`: add variant, `From`, and classification.

```rust
// in enum WalletKitError:
    /// A chain read failed (RPC transport or on-chain decode).
    #[error(transparent)]
    Read(crate::core::deps::ReadError),
```
```rust
// in kind():
    Self::Read(ReadError::Rpc(e)) => rpc_kind(e),
    Self::Read(ReadError::Decode { .. }) => ErrorKind::Terminal,
```
```rust
impl From<crate::core::deps::ReadError> for WalletKitError {
    fn from(e: crate::core::deps::ReadError) -> Self {
        Self::Read(e)
    }
}
```
Add `ReadError` to the `use crate::core::deps::{...}` import and (if `kind()` matches on it) bring the enum into scope with `use crate::core::deps::ReadError;` inside `kind()` or fully-qualify.

- [ ] **Step 7: Write the integration test (the failing test)**

Create `tests/read.rs`. Guard on anvil availability the same way the localnet suite does (skip cleanly). This test proves the regression-worthy behavior: typed reads, and **per-token failure isolation** in `balances`.

```rust
mod support;
use support::Localnet;

use alloy_primitives::{Address, U256};
use std::sync::Arc;
use walletkit::adapters::{RpcReadClient, Transport};
use walletkit::core::deps::ReadClient;

// Spawn a bare anvil (no wallet needed) + a read client over it, or skip.
async fn read_client() -> Option<(Localnet, RpcReadClient, Address)> {
    let net = Localnet::spawn_bare().await?; // add this thin ctor (see note)
    let transport = Transport::single(net.endpoint()).ok()?;
    let deployer = net.account(0);
    Some((net, RpcReadClient::new(transport.provider()), deployer))
}

#[tokio::test]
async fn reads_erc20_and_isolates_a_reverting_token() {
    let Some((net, read, deployer)) = read_client().await else {
        eprintln!("skipping: anvil unavailable");
        return;
    };
    let token = net.deploy_mock_erc20(0).await;

    // Native + ERC-20 metadata + balance.
    assert!(read.native_balance(deployer).await.unwrap() > U256::ZERO);
    let md = read.erc20_metadata(token).await.unwrap();
    assert_eq!((md.name.as_str(), md.symbol.as_str(), md.decimals), ("Mock", "MOCK", 18));
    let bal = read.erc20_balance(token, deployer).await.unwrap();
    assert_eq!(bal, U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18)));

    // Allowance after an on-chain approve.
    let spender = net.account(1);
    let approve = walletkit::adapters::read::IERC20::approveCall { spender, amount: U256::from(42u64) };
    // (encode via the mock binding in support instead if IERC20 isn't public; see note)

    // balances(): native folds in; a non-token address reverts in isolation.
    let not_a_token = Address::from([0x99; 20]);
    let overview = read.balances(deployer, &[token, not_a_token]).await.unwrap();
    assert!(overview.native > U256::ZERO);
    assert_eq!(overview.tokens[0].token, token);
    assert!(overview.tokens[0].balance.as_ref().unwrap() > &U256::ZERO);
    assert_eq!(overview.tokens[1].token, not_a_token);
    assert!(overview.tokens[1].balance.is_err(), "EOA has no balanceOf → isolated Err");

    drop(net);
}
```

Two harness additions this test implies (fold into `tests/support/mod.rs`): `Localnet::spawn_bare()` (anvil only, no wallet/store — factor out of `spawn_on`), `Localnet::endpoint()` accessor, `Localnet::account(i)` (address of anvil account `i`). For the allowance assertion, drive `approve` through `support::MockErc20::approveCall.abi_encode()` + `net.send_tx(0, token, ...)`, then assert `read.erc20_allowance(token, deployer, spender)`. Keep the test to the two behaviors that can regress (typed decode + failure isolation); do **not** assert every getter separately if it bloats — one metadata + one balance + the isolation case earns its place.

- [ ] **Step 8: Run the test to verify it fails (not yet compiled/implemented)**

Run: `cargo test --test read`
Expected: FAIL to compile until Steps 4–6 land, then the assertions pass once implemented. Iterate Steps 4–7 until green.

- [ ] **Step 9: Run the full gate and report**

```
cargo fmt --check
cargo clippy --all-targets --all-features
cargo clippy --all-targets --no-default-features
cargo test
cargo test --test read          # anvil-gated; shows pass or clean skip
```
Report the real output. Confirm green **with and without** `--no-default-features` (the read path has no `tracing`/`redb` coupling).

- [ ] **Step 10: Commit (after approval)**

```bash
git add Cargo.toml Cargo.lock src/core/deps/ src/adapters/ src/error.rs tests/
git commit -m "feat(read): ReadClient port + RpcReadClient adapter (multicall3 balances)"
```

---

### Task 2: `Rpc` simulation surface — `call` → `Simulated`, `create_access_list`

**Files:**
- Modify: `src/core/deps/rpc.rs` (add `Simulated` enum + two trait methods)
- Modify: `src/adapters/transport/mod.rs` (impl both; revert-data extraction)
- Modify: `src/testutils.rs` (extend `MockRpc` with default impls)
- Create: `tests/preview.rs` (first scenario: `Transport::call` revert vs return) — extended in Task 3

**Interfaces:**
- Consumes: Task 1's `Transport` + mock fixture (`deploy_mock_erc20`, `revertWith` selector).
- Produces:
  - `enum Simulated { Returned(Bytes), Reverted(Bytes) }` in `rpc.rs`.
  - `Rpc::call(&self, request: &TransactionRequest) -> Result<Simulated, RpcError>`
  - `Rpc::create_access_list(&self, request: &TransactionRequest) -> Result<AccessListResult, RpcError>` (`AccessListResult` = `alloy_rpc_types_eth::AccessListResult`).

- [ ] **Step 1: Extend the `Rpc` port**

In `src/core/deps/rpc.rs`, add the outcome type and two methods (reuse alloy's `AccessListResult` — a concrete data type, allowed across the port):

```rust
use alloy_rpc_types_eth::AccessListResult;

/// The result of an `eth_call` simulation: the call either returned data or
/// reverted with data. A revert is a normal outcome here (preview), not an error;
/// only a transport/node failure is an `Err`.
pub enum Simulated {
    Returned(Bytes),
    Reverted(Bytes),
}
```
```rust
// inside trait Rpc:
    /// `eth_call` at latest: returns the call's output, or its revert data. A
    /// transport/node failure (not a contract revert) is the only `Err`.
    async fn call(&self, request: &TransactionRequest) -> Result<Simulated, RpcError>;
    /// `eth_createAccessList`: the EIP-2930 list plus the addresses/slots touched.
    async fn create_access_list(
        &self,
        request: &TransactionRequest,
    ) -> Result<AccessListResult, RpcError>;
```
Re-export in `src/core/deps/mod.rs`: `pub use rpc::{Rpc, RpcError, Simulated};`.

- [ ] **Step 2: Implement in the Transport adapter**

In `src/adapters/transport/mod.rs`, add the two methods. The revert-data extraction reuses alloy's `as_error_resp()?.as_revert_data()` — do **not** hand-parse JSON:

```rust
use crate::core::deps::Simulated;
use alloy_rpc_types_eth::AccessListResult;

    async fn call(&self, request: &TransactionRequest) -> Result<Simulated, RpcError> {
        match self.provider.call(request.clone()).await {
            Ok(data) => Ok(Simulated::Returned(data)),
            Err(e) => match e.as_error_resp().and_then(|p| p.as_revert_data()) {
                Some(revert) => Ok(Simulated::Reverted(revert)),
                None => Err(rpc_err(e)),
            },
        }
    }

    async fn create_access_list(
        &self,
        request: &TransactionRequest,
    ) -> Result<AccessListResult, RpcError> {
        self.provider
            .create_access_list(request)
            .await
            .map_err(rpc_err)
    }
```

> `provider.call(tx)` returns `EthCall<..>` which awaits to `Result<Bytes, RpcError<TransportErrorKind>>`; on a contract revert the error carries the revert bytes, surfaced by `as_error_resp().as_revert_data()`. `create_access_list(&req)` returns `Result<AccessListResult, _>`. Both are alloy `Provider` methods — no new mechanics.

- [ ] **Step 3: Extend `MockRpc`**

In `src/testutils.rs`, add default impls so existing unit tests keep compiling (they don't preview): `call` returns `Ok(Simulated::Returned(Bytes::new()))`, `create_access_list` returns `Ok(AccessListResult { access_list: Default::default(), gas_used: U256::ZERO, error: None })`. If preview-specific unit tests later need to inject revert bytes, add optional fields to `MockRpc` **then** (YAGNI now).

- [ ] **Step 4: Write the failing adapter test**

In `tests/preview.rs` (shared with Task 3), assert `Transport::call` distinguishes a return from a revert, reusing Task 1's mock:

```rust
mod support;
use support::Localnet;
use std::sync::Arc;
use walletkit::adapters::Transport;
use walletkit::core::deps::{Rpc, Simulated};

#[tokio::test]
async fn transport_call_returns_revert_data_for_a_reverting_call() {
    let Some(net) = Localnet::spawn_bare().await else { return };
    let token = net.deploy_mock_erc20(0).await;
    let transport = Transport::single(net.endpoint()).unwrap();

    // revertWith() reverts with Error("nope").
    let req = alloy_rpc_types_eth::TransactionRequest::default()
        .to(token)
        .input(support::MockErc20::revertWithCall {}.abi_encode().into());
    let out = Rpc::call(&transport, &req).await.unwrap();
    assert!(matches!(out, Simulated::Reverted(_)), "a reverting call yields Reverted, not Err");
}
```

- [ ] **Step 5: Run to verify fail → implement → pass**

Run: `cargo test --test preview -- transport_call_returns_revert_data_for_a_reverting_call`
Expected: FAIL to compile until Steps 1–2 land; then PASS.

- [ ] **Step 6: Full gate + report** (`cargo fmt --check`, `cargo clippy --all-targets` with and without `--no-default-features`, `cargo test`). Report real output.

- [ ] **Step 7: Commit (after approval)**

```bash
git add src/core/deps/rpc.rs src/adapters/transport/mod.rs src/testutils.rs tests/
git commit -m "feat(rpc): eth_call simulation (Simulated) + create_access_list on the Rpc port"
```

---

### Task 3: `TxPreview` + `Wallet::dry_run` (RevertReason decode)

**Files:**
- Create: `src/core/wallet/preview.rs`
- Modify: `src/core/wallet/mod.rs` (add `mod preview;` + re-exports)
- Modify: `src/facade.rs` (`Wallet::dry_run`)
- Modify: `tests/preview.rs` (localnet dry_run scenarios)

**Interfaces:**
- Consumes: `Rpc::{call, create_access_list, estimate_gas}`, `Simulated` (Task 2); `TxIntent`, `TransactionInput`/`TransactionRequest` build (mirrors `TransactionManager::send`).
- Produces:
  - `struct TxPreview { gas_estimate: Option<u64>, outcome: SimOutcome, access_list: Option<AccessList>, return_data: Bytes }`
  - `enum SimOutcome { Success, Revert(RevertReason) }`
  - `enum RevertReason { Error(String), Panic(u64), Custom { selector: [u8; 4], data: Bytes }, Unknown(Bytes) }`
  - free fn `decode_revert(data: &Bytes) -> RevertReason`
  - `Wallet::dry_run(&self, intent: &TxIntent) -> Result<TxPreview, WalletKitError>`

- [ ] **Step 1: Write the `RevertReason` unit tests first (pure logic — earns its place)**

In `src/core/wallet/preview.rs`, a `#[cfg(test)] mod tests` asserting the decode of crafted bytes:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use alloy_sol_types::{Revert, Panic, SolError};

    #[test]
    fn decodes_error_string_panic_custom_and_unknown() {
        // Error(string)
        let e = Bytes::from(Revert::from("boom").abi_encode());
        assert!(matches!(decode_revert(&e), RevertReason::Error(s) if s == "boom"));

        // Panic(uint256) with code 0x11 (arithmetic overflow)
        let p = Bytes::from(Panic { code: alloy_primitives::U256::from(0x11) }.abi_encode());
        assert!(matches!(decode_revert(&p), RevertReason::Panic(0x11)));

        // Unknown custom error selector + tail
        let mut c = vec![0xaa, 0xbb, 0xcc, 0xdd];
        c.extend_from_slice(&[0u8; 32]);
        assert!(matches!(decode_revert(&Bytes::from(c)),
            RevertReason::Custom { selector: [0xaa, 0xbb, 0xcc, 0xdd], .. }));

        // Empty / opaque
        assert!(matches!(decode_revert(&Bytes::new()), RevertReason::Unknown(_)));
    }
}
```

> Verify `Panic`'s field name in `alloy_sol_types` (likely `code: U256`; convert with `.to::<u64>()`). If the constructor differs, use `Panic::abi_decode` round-trips instead of struct literals in the test.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p walletkit preview::tests::decodes_error_string_panic_custom_and_unknown`
Expected: FAIL ("cannot find function `decode_revert`").

- [ ] **Step 3: Implement `preview.rs`**

```rust
//! `TxPreview` — an RPC-only pre-sign simulation: gas estimate, success/revert with
//! a decoded reason, EIP-2930 access list, and raw return data. Composed from
//! `eth_call` + `eth_estimateGas` + `eth_createAccessList`; never signs or mutates
//! state. Gas is advisory (a dry-run is a lower bound). A revert is a *successful*
//! preview with a `Revert` outcome, not an error.

use crate::core::deps::{Rpc, RpcError, Simulated};
use crate::core::wallet::TxIntent;
use alloy_primitives::Bytes;
use alloy_rpc_types_eth::{AccessList, TransactionInput, TransactionRequest};
use alloy_sol_types::{Panic, Revert, SolError};

pub struct TxPreview {
    /// `eth_estimateGas` — advisory; `None` when the tx would revert (no meaningful estimate).
    pub gas_estimate: Option<u64>,
    pub outcome: SimOutcome,
    /// EIP-2930 access list (also reveals the addresses/slots the call touches).
    pub access_list: Option<AccessList>,
    /// Raw `eth_call` return data; the caller ABI-decodes if it expects a value.
    pub return_data: Bytes,
}

pub enum SimOutcome {
    Success,
    Revert(RevertReason),
}

/// A decoded `eth_call` revert. Standard selectors are named; anything else keeps its
/// raw bytes for the caller to interpret.
pub enum RevertReason {
    /// `Error(string)` — `0x08c379a0`.
    Error(String),
    /// `Panic(uint256)` — `0x4e487b71`; carries the panic code.
    Panic(u64),
    /// A contract's custom error: 4-byte selector + ABI tail.
    Custom { selector: [u8; 4], data: Bytes },
    /// Empty or non-decodable revert data.
    Unknown(Bytes),
}

/// Decode raw revert bytes into a [`RevertReason`] (RPC-only; no provider needed).
pub fn decode_revert(data: &Bytes) -> RevertReason {
    if data.len() < 4 {
        return RevertReason::Unknown(data.clone());
    }
    let selector: [u8; 4] = data[..4].try_into().unwrap_or([0; 4]);
    match selector {
        Revert::SELECTOR => match Revert::abi_decode(data) {
            Ok(r) => RevertReason::Error(r.reason),
            Err(_) => RevertReason::Unknown(data.clone()),
        },
        Panic::SELECTOR => match Panic::abi_decode(data) {
            Ok(p) => RevertReason::Panic(p.code.to::<u64>()),
            Err(_) => RevertReason::Unknown(data.clone()),
        },
        sel => RevertReason::Custom { selector: sel, data: data.clone() },
    }
}

/// Simulate an intent over `rpc` without signing: `eth_call` (outcome + return),
/// `eth_estimateGas` (advisory), `eth_createAccessList` (access list). A revert on the
/// call is a `Revert` outcome, not an error; gas/access-list failures degrade to `None`.
pub async fn dry_run(rpc: &dyn Rpc, intent: &TxIntent) -> Result<TxPreview, RpcError> {
    let request = TransactionRequest {
        from: Some(intent.account),
        to: Some(intent.to),
        value: Some(intent.value),
        input: TransactionInput::new(intent.input.clone()),
        ..Default::default()
    };

    let (outcome, return_data) = match rpc.call(&request).await? {
        Simulated::Returned(data) => (SimOutcome::Success, data),
        Simulated::Reverted(data) => (SimOutcome::Revert(decode_revert(&data)), data),
    };

    // Gas/access-list are advisory extras: a revert makes estimate_gas fail, which is
    // expected — don't let it mask the (already-known) outcome.
    let gas_estimate = rpc.estimate_gas(&request).await.ok();
    let access_list = rpc.create_access_list(&request).await.ok().map(|r| r.access_list);

    Ok(TxPreview { gas_estimate, outcome, access_list, return_data })
}
```

Wire in `src/core/wallet/mod.rs`: `mod preview;` and `pub use preview::{decode_revert, RevertReason, SimOutcome, TxPreview};` (keep `dry_run` internal — the facade exposes it).

- [ ] **Step 4: Run the unit test to verify it passes**

Run: `cargo test -p walletkit preview::tests::decodes_error_string_panic_custom_and_unknown`
Expected: PASS.

- [ ] **Step 5: Add `Wallet::dry_run`**

In `src/facade.rs`:

```rust
use crate::core::wallet::{dry_run, TxPreview};

    /// Simulate an intent without signing or broadcasting: gas (advisory), success or
    /// decoded revert reason, access list, and return data. A would-revert tx yields a
    /// `TxPreview` with a `Revert` outcome — not an error.
    pub async fn dry_run(&self, intent: &TxIntent) -> Result<TxPreview, WalletKitError> {
        Ok(dry_run(self.rpc.as_ref(), intent).await?)
    }
```

`Wallet` must hold `rpc: Arc<dyn Rpc>` — check `facade.rs`; if the field isn't retained today (only `manager`/`executor` are), add `rpc` to the `Wallet` struct and set it in `WalletBuilder::build` (it already has `self.rpc`). Re-export `dry_run` as `pub(crate)` from `core::wallet` for the facade. `WalletKitError::Rpc` already exists, so `?` on `RpcError` maps cleanly — **no new error variant needed** (preview failures are purely transport; reverts aren't errors).

- [ ] **Step 6: Add the localnet dry_run scenarios (failing test)**

Append to `tests/preview.rs`: a successful transfer preview (Success + gas + access list) and a would-revert preview (Revert with decoded "nope"), asserting no state change. Build a `Wallet` via the existing `Localnet::spawn_on` (it returns a wallet) or `spawn_bare` + a permissive policy; simplest is to preview via the wallet's `dry_run`:

```rust
use walletkit::core::wallet::{SimOutcome, RevertReason, TxIntent};

#[tokio::test]
async fn dry_run_previews_success_and_decodes_revert_without_state_change() {
    let Some(net) = Localnet::spawn_on(support::Backend::InMemory, 0, 1).await else { return };
    let token = net.deploy_mock_erc20(0).await;
    let wallet = net.wallet();          // existing accessor
    let account = wallet.account();

    // A reverting call → Revert(Error("nope")), gas None, no state touched.
    let revert_intent = TxIntent {
        chain_id: net.chain_id(),
        account,
        to: alloy_primitives::TxKind::Call(token),
        value: alloy_primitives::U256::ZERO,
        input: support::MockErc20::revertWithCall {}.abi_encode().into(),
        purpose: None,
    };
    let p = wallet.dry_run(&revert_intent).await.unwrap();
    assert!(matches!(p.outcome, SimOutcome::Revert(RevertReason::Error(ref s)) if s == "nope"));
    assert!(p.gas_estimate.is_none());

    // A plain value transfer to a fresh EOA → Success + a gas estimate.
    let ok_intent = TxIntent {
        chain_id: net.chain_id(),
        account,
        to: alloy_primitives::TxKind::Call(Address::from([0x77; 20])),
        value: alloy_primitives::U256::from(1u64),
        input: Default::default(),
        purpose: None,
    };
    let p = wallet.dry_run(&ok_intent).await.unwrap();
    assert!(matches!(p.outcome, SimOutcome::Success));
    assert!(p.gas_estimate.unwrap() >= 21_000);

    // No handle was created — dry_run never touches the store/executor.
    drop(net);
}
```

Add `Localnet::wallet(&self) -> Arc<Wallet>` if not already present (the harness builds one in `spawn_on`).

- [ ] **Step 7: Run → implement gaps → pass.** `cargo test --test preview`. Expected: PASS (anvil-gated skip otherwise).

- [ ] **Step 8: Full gate + report** (fmt/clippy ±default-features/test). Report real output.

- [ ] **Step 9: Commit (after approval)**

```bash
git add src/core/wallet/ src/facade.rs tests/preview.rs
git commit -m "feat(preview): TxPreview + Wallet::dry_run with decoded revert reasons"
```

---

### Task 4: `EnsResolver` port + `RpcEnsResolver` adapter (alloy-ens)

**Files:**
- Modify: `Cargo.toml` (add `alloy-ens = "2.4.1"`)
- Create: `src/core/deps/ens.rs`
- Create: `src/adapters/ens.rs`
- Modify: `src/core/deps/mod.rs`, `src/adapters/mod.rs` (wiring)
- Modify: `src/error.rs` (`WalletKitError::Ens` + `From` + `kind()`)

**Interfaces:**
- Consumes: `Transport::provider()` (`DynProvider`), `alloy_ens::{ProviderEnsExt, EnsError as AlloyEnsError, namehash}`.
- Produces:
  - `trait EnsResolver: Send + Sync` — `resolve_name`, `reverse_lookup`, `text_record`, `avatar` (default = `text_record(name, "avatar")`), each returning `Result<Option<_>, EnsError>`.
  - `enum EnsError` (`#[non_exhaustive]`): `Rpc(RpcError)`, `Resolution { detail: String }`.
  - `RpcEnsResolver::new(provider: DynProvider) -> Self`.

- [ ] **Step 1: Add dependency**

`Cargo.toml`: `alloy-ens = "2.4.1"`. (Pulls no new heavy transitive deps — alloy-contract/provider are already present.)

- [ ] **Step 2: Write the port**

Create `src/core/deps/ens.rs`. Reuse: alloy-ens does namehash + registry→resolver lookups; the port stays four verbs and maps not-found to `None`:

```rust
use crate::core::deps::RpcError;
use alloy_primitives::Address;
use async_trait::async_trait;

/// ENS name resolution over plain RPC (registry → resolver). `None` means "no record"
/// (unregistered, no resolver, or a reverse name that fails forward-verification);
/// only transport/operational failures are `Err`.
#[async_trait]
pub trait EnsResolver: Send + Sync {
    async fn resolve_name(&self, name: &str) -> Result<Option<Address>, EnsError>;
    async fn reverse_lookup(&self, address: Address) -> Result<Option<String>, EnsError>;
    async fn text_record(&self, name: &str, key: &str) -> Result<Option<String>, EnsError>;
    async fn avatar(&self, name: &str) -> Result<Option<String>, EnsError> {
        self.text_record(name, "avatar").await
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnsError {
    #[error(transparent)]
    Rpc(RpcError),
    /// The name needs EIP-3668 CCIP-Read (an offchain/L2 name — Basenames `*.base.eth`,
    /// `*.cb.id`, L2 subnames). Strict RPC does not follow the gateway hop; surfaced
    /// distinctly (not a generic failure) so a caller can opt into a future CCIP feature.
    #[error("ens name requires offchain CCIP-Read resolution")]
    OffchainLookupRequired,
    /// An ENS-specific operational failure (bad resolver, malformed name). Distinct from
    /// "no record", which is `Ok(None)`.
    #[error("ens resolution failed: {detail}")]
    Resolution { detail: String },
}
```
Wire in `src/core/deps/mod.rs`.

- [ ] **Step 3: Write the adapter**

Create `src/adapters/ens.rs`. Delegate to `ProviderEnsExt`; map alloy-ens's not-found errors to `Ok(None)`; **forward-verify** reverse lookups (alloy-ens `lookup_address` resolves the reverse name but callers must confirm it maps back):

```rust
//! `RpcEnsResolver` — ENS over `alloy-ens`'s `ProviderEnsExt`. Reverse lookups are
//! forward-verified (a claimed name must resolve back to the same address), and
//! not-found is normalized to `Ok(None)`.

use crate::core::deps::{EnsError, EnsResolver};
use alloy_ens::{EnsError as AlloyEnsError, ProviderEnsExt};
use alloy_primitives::Address;
use alloy_provider::DynProvider;
use async_trait::async_trait;

pub struct RpcEnsResolver {
    provider: DynProvider,
}

impl RpcEnsResolver {
    pub fn new(provider: DynProvider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl EnsResolver for RpcEnsResolver {
    async fn resolve_name(&self, name: &str) -> Result<Option<Address>, EnsError> {
        match self.provider.resolve_name(name).await {
            Ok(addr) => Ok(Some(addr)),
            Err(e) => not_found_to_none(e),
        }
    }

    async fn reverse_lookup(&self, address: Address) -> Result<Option<String>, EnsError> {
        let name = match self.provider.lookup_address(&address).await {
            Ok(name) => name,
            Err(e) => return not_found_to_none(e),
        };
        // Forward-verify: a reverse record is only trustworthy if the name resolves
        // back to the same address (ENS spec; guards spoofed reverse entries).
        match self.provider.resolve_name(&name).await {
            Ok(forward) if forward == address => Ok(Some(name)),
            Ok(_) => Ok(None),
            Err(e) => not_found_to_none(e),
        }
    }

    async fn text_record(&self, name: &str, key: &str) -> Result<Option<String>, EnsError> {
        match self.provider.lookup_txt(name, key).await {
            Ok(v) if v.is_empty() => Ok(None),
            Ok(v) => Ok(Some(v)),
            Err(e) => not_found_to_none(e),
        }
    }
}

/// Map an alloy-ens error: "no resolver / not found" → empty result; an EIP-3668
/// `OffchainLookup` revert → the typed `OffchainLookupRequired`; anything else → a real
/// `EnsError`. Match on the variant if alloy-ens exposes one; otherwise detect the
/// `OffchainLookup` selector (`0x556f1830`) in the revert data before the default arm.
fn not_found_to_none<T>(e: AlloyEnsError) -> Result<Option<T>, EnsError> {
    match e {
        AlloyEnsError::ResolverNotFound(_) | AlloyEnsError::ReverseRegistrarNotFound => Ok(None),
        other if is_offchain_lookup(&other) => Err(EnsError::OffchainLookupRequired),
        other => Err(EnsError::Resolution { detail: other.to_string() }),
    }
}
```

Implement `is_offchain_lookup(&AlloyEnsError) -> bool`: prefer a dedicated alloy-ens variant if 2.4.1 exposes one; otherwise inspect the wrapped revert data for the EIP-3668 `OffchainLookup(...)` selector `0x556f1830`. If neither is reachable, leave offchain names mapping to `Resolution` and note the limitation — never silently follow the gateway.

> **Correctness notes (the load-bearing bits `alloy-ens` does *not* give us):**
> - **Reverse forward-verification is the primary spoofing guard** and is implemented above (resolve the claimed name back, compare addresses → mismatch is `None`). Before forward-resolving, **normalize** the candidate name (ENSIP-15/UTS-46). Full normalization needs a crate (`ens-normalize`); without it a non-normalized name simply forward-resolves differently and fails the address check, so forward-verify stays safe. Add the normalization crate only if a consumer needs lenient input (YAGNI); otherwise document that callers pass normalized names.
> - Confirm the exact `AlloyEnsError` variant names against `alloy-ens` 2.4.1 (`ResolverNotFound(String)`, `ReverseRegistrarNotFound` were seen in its source). If a `no records`/`NotFound` variant differs, extend the match; keep the default arm mapping to `Resolution`.
> - `avatar` returns the raw text record; typed NFT-avatar resolution (ENSIP-12) is a deferred, separately-named method — do not add it now.

Wire in `src/adapters/mod.rs`: `pub mod ens;` + `pub use ens::RpcEnsResolver;`.

- [ ] **Step 4: Public error mapping**

`src/error.rs`: add `Ens(crate::core::deps::EnsError)` variant (`#[error(transparent)]`), `From<EnsError>`, and `kind()` arms (`Ens(EnsError::Rpc(e)) => rpc_kind(e)`, `Ens(EnsError::Resolution { .. }) => ErrorKind::Terminal`).

- [ ] **Step 5: Test — the None-mapping/forward-verify logic (earns its place); live resolution is env-gated**

The regression-worthy logic is `not_found_to_none` + the forward-verify branch. Pure ENS resolution needs a mainnet fork (real registry), so gate a live test behind an env var and keep it optional; do **not** hand-roll a fake registry (low value, high cost). Add one env-gated integration test in `tests/ens.rs`:

```rust
mod support;
// Runs only when WALLETKIT_ENS_FORK_RPC is set to a mainnet RPC URL; otherwise skips.
#[tokio::test]
async fn resolves_and_reverse_verifies_vitalik_eth() {
    let Ok(url) = std::env::var("WALLETKIT_ENS_FORK_RPC") else {
        eprintln!("skipping: set WALLETKIT_ENS_FORK_RPC to run ENS live test");
        return;
    };
    use walletkit::adapters::{RpcEnsResolver, Transport};
    use walletkit::core::deps::EnsResolver;
    let transport = Transport::single(url.parse().unwrap()).unwrap();
    let ens = RpcEnsResolver::new(transport.provider());
    let addr = ens.resolve_name("vitalik.eth").await.unwrap().expect("resolves");
    let back = ens.reverse_lookup(addr).await.unwrap();
    assert_eq!(back.as_deref(), Some("vitalik.eth"));
}
```

If a unit-level assertion on `not_found_to_none` is cheap to express (constructing an `AlloyEnsError::ResolverNotFound(..)` and asserting `Ok(None)`), add it; otherwise the env-gated test plus clippy cover the mapping. Prefer the smallest test that proves the mapping can't regress.

- [ ] **Step 6: Run → implement → pass.** `cargo test --test ens` (skips without the env var). `cargo build` must succeed with the new crate.

- [ ] **Step 7: Full gate + report** (fmt/clippy ±default-features/test).

- [ ] **Step 8: Commit (after approval)**

```bash
git add Cargo.toml Cargo.lock src/core/deps/ens.rs src/adapters/ens.rs src/core/deps/mod.rs src/adapters/mod.rs src/error.rs tests/
git commit -m "feat(ens): EnsResolver port + RpcEnsResolver adapter (forward-verified reverse)"
```

---

### Task 5: Enrichment seam — `TokenMetadataSource` + `PriceSource` (feature `enrich`)

**Files:**
- Modify: `Cargo.toml` (feature `enrich`)
- Create: `src/core/deps/enrich.rs` (ports; gated)
- Create: `src/adapters/enrich/mod.rs`, `src/adapters/enrich/token_list.rs`, `src/adapters/enrich/chainlink.rs`
- Modify: `src/core/deps/mod.rs`, `src/adapters/mod.rs` (gated wiring)

**Interfaces:**
- Consumes: `Erc20Metadata` (Task 1), `DynProvider`, `alloy-contract` (`#[sol(rpc)]` `AggregatorV3Interface`).
- Produces (all behind `#[cfg(feature = "enrich")]`):
  - `trait TokenMetadataSource: Send + Sync` — `metadata(chain_id, token) -> Result<Option<Erc20Metadata>, EnrichError>`
  - `trait PriceSource: Send + Sync` — `price(chain_id, token, vs: Currency) -> Result<Option<Price>, EnrichError>`
  - `struct Price { value: U256, decimals: u8, updated_at: u64 }`, `enum Currency { Usd }`
  - `enum EnrichError` (`Rpc`, `List { detail: String }`, `Stale { age_secs: u64 }`)
  - `TokenListSource::from_json(bytes: &[u8]) -> Result<Self, EnrichError>` + `TokenListSource::with_fallback(read: Arc<dyn ReadClient>)` (on-chain gap fill, bytes32-tolerant via `ReadClient::erc20_metadata`)
  - `struct FeedConfig { address: Address, heartbeat_secs: u64 }`
  - `ChainlinkPrice::new(provider: DynProvider, clock: Arc<dyn Clock>, feeds: HashMap<(u64, Address), FeedConfig>, grace_secs: u64)` — **staleness is per-feed heartbeat + grace, not a global constant** (heartbeats vary 3600s↔86400s per feed/chain); `now` comes from the injected `Clock` (no ambient time).

- [ ] **Step 1: Add the feature**

`Cargo.toml` `[features]`: `enrich = []` (no new deps — token-list uses `serde_json` already present; Chainlink uses `alloy-contract` added in Task 1). Add `enrich` to a CI matrix note but **not** to `default`.

- [ ] **Step 2: Write the ports (gated)**

Create `src/core/deps/enrich.rs`. Gate the whole module so a no-`enrich` build compiles nothing here:

```rust
use crate::core::deps::{Erc20Metadata, RpcError, ReadClient};
use alloy_primitives::{Address, U256};
use async_trait::async_trait;

/// Off-chain / vendor-neutral token metadata (e.g. a Uniswap token-list). Independent
/// of the on-chain `ReadClient`; a caller composes the two.
#[async_trait]
pub trait TokenMetadataSource: Send + Sync {
    async fn metadata(
        &self,
        chain_id: u64,
        token: Address,
    ) -> Result<Option<Erc20Metadata>, EnrichError>;
}

/// A price feed for a token in a quote currency. RPC-compatible adapters (Chainlink)
/// ship in-crate; vendor HTTP adapters (CoinGecko, …) are the same trait, deferred.
#[async_trait]
pub trait PriceSource: Send + Sync {
    async fn price(
        &self,
        chain_id: u64,
        token: Address,
        vs: Currency,
    ) -> Result<Option<Price>, EnrichError>;
}

pub struct Price {
    pub value: U256,
    pub decimals: u8,
    /// Feed `updatedAt` (unix seconds) — a caller checks freshness against its clock.
    pub updated_at: u64,
}

pub enum Currency {
    Usd,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnrichError {
    #[error(transparent)]
    Rpc(RpcError),
    #[error("token list error: {detail}")]
    List { detail: String },
    /// A price-feed round failed validation (non-positive answer, zero/future timestamp, …).
    #[error("invalid price feed round: {detail}")]
    Feed { detail: String },
    #[error("price feed stale by {age_secs}s")]
    Stale { age_secs: u64 },
}
```
Gate in `src/core/deps/mod.rs`: `#[cfg(feature = "enrich")] pub mod enrich;` + gated re-exports.

- [ ] **Step 3: Token-list adapter + unit test (parse + lookup + on-chain fallback)**

Create `src/adapters/enrich/token_list.rs`. The list is a Uniswap-schema JSON; parse once into a `(chain_id, address) → Erc20Metadata` map (no RPC at read time). If a token is missing and a `ReadClient` fallback is configured, fill from chain.

Write the failing unit test first (pure logic — parse + fallback path):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_uniswap_schema_and_looks_up_by_chain_and_address() {
        let json = br#"{"tokens":[
            {"chainId":1,"address":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","name":"USD Coin","symbol":"USDC","decimals":6}
        ]}"#;
        let src = TokenListSource::from_json(json).unwrap();
        let md = src.lookup(1, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".parse().unwrap()).unwrap();
        assert_eq!((md.symbol.as_str(), md.decimals), ("USDC", 6));
        assert!(src.lookup(1, alloy_primitives::Address::ZERO).is_none());
    }
}
```

Implement: `#[derive(Deserialize)]` for the list entry (`chainId`, `address`, `name`, `symbol`, `decimals`), build a `HashMap<(u64, Address), Erc20Metadata>`, `from_json` maps serde errors to `EnrichError::List`. `metadata()` returns the map hit; on a miss with a configured `ReadClient`, call `read.erc20_metadata(token)` and map `ReadError` → `EnrichError::Rpc`/`List`. The `lookup` sync helper (used by the test) is the pure core; `metadata` wraps it with the async fallback. Keep the on-chain fallback out of the unit test (it needs a provider) — the map lookup is the regression-worthy logic.

- [ ] **Step 4: Chainlink adapter + unit test (scale + staleness — pure math)**

Create `src/adapters/enrich/chainlink.rs`. `sol!(#[sol(rpc)] AggregatorV3Interface { latestRoundData() → (uint80,int256,uint256,uint256,uint80); decimals() → uint8; })`. `price()` looks up the feed address for `(chain_id, token)`, reads `latestRoundData` + `decimals`, and applies **pure** scale/staleness logic. Extract that logic into a free fn and unit-test it (no provider needed):

```rust
use alloy_primitives::{I256, U256};

/// One decoded `latestRoundData` round.
struct Round { round_id: U256, answer: I256, updated_at: u64 }

/// Turn a raw Chainlink round into a `Price`, or reject it. Pure: no I/O. Validates the
/// round (non-zero round, sane timestamps, positive answer) and enforces the FEED's own
/// heartbeat + grace — not a single global max-age (heartbeats vary widely per feed/chain).
fn to_price(round: Round, decimals: u8, now: u64, heartbeat_secs: u64, grace_secs: u64)
    -> Result<Price, EnrichError>
{
    if round.round_id.is_zero() || round.updated_at == 0 || round.updated_at > now {
        return Err(EnrichError::Feed { detail: "invalid round".into() });
    }
    // A non-positive answer is a feed fault (stale/misconfigured), not a real price.
    if round.answer <= I256::ZERO {
        return Err(EnrichError::Feed { detail: "non-positive price".into() });
    }
    let age = now - round.updated_at;
    if age > heartbeat_secs.saturating_add(grace_secs) {
        return Err(EnrichError::Stale { age_secs: age });
    }
    let value = U256::try_from(round.answer).map_err(|_| EnrichError::Feed { detail: "price overflow".into() })?;
    Ok(Price { value, decimals, updated_at: round.updated_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn round(answer: i128, updated_at: u64) -> Round {
        Round { round_id: U256::from(1u64), answer: I256::try_from(answer).unwrap(), updated_at }
    }
    #[test]
    fn fresh_round_passes_stale_and_bad_rounds_reject() {
        // Fresh: age 30s < heartbeat 3600 + grace 60.
        let p = to_price(round(200_000_000_000, 1_000), 8, 1_030, 3_600, 60).unwrap();
        assert_eq!((p.value, p.decimals), (U256::from(200_000_000_000u64), 8));
        // Stale: age 4000s > 3600 + 60.
        assert!(matches!(to_price(round(1, 1_000), 8, 5_000, 3_600, 60), Err(EnrichError::Stale { .. })));
        // Non-positive answer rejected.
        assert!(matches!(to_price(round(0, 1_000), 8, 1_030, 3_600, 60), Err(EnrichError::Feed { .. })));
        // Future/zero timestamp rejected.
        assert!(matches!(to_price(round(1, 0), 8, 1_030, 3_600, 60), Err(EnrichError::Feed { .. })));
    }
}
```

`price()` (async) reads `latestRoundData` + `decimals` via the contract instance, packs them into `Round`, looks up the `(chain_id, token) → FeedConfig`, and delegates to `to_price` with `now` from the injected `Clock` (reuse `core::deps::Clock` — never call `SystemTime` directly) and the feed's `heartbeat_secs`. A missing feed → `Ok(None)`; a stale/invalid round → `Err`. Map contract errors to `EnrichError::Rpc`. Do NOT use the deprecated `answeredInRound`/`latestAnswer`.

> Confirm `alloy_primitives::I256`, `I256::ZERO`, and `U256::try_from(I256)`; if `try_from` is absent, guard `answer.is_negative()` then `answer.into_raw()`. `roundId` is `uint80` on-chain — decode into `U256` and check `is_zero()`.

- [ ] **Step 5: Adapter module wiring**

`src/adapters/enrich/mod.rs`: `pub mod token_list; pub mod chainlink; pub use token_list::TokenListSource; pub use chainlink::ChainlinkPrice;`. In `src/adapters/mod.rs`: `#[cfg(feature = "enrich")] pub mod enrich;` + gated re-exports.

- [ ] **Step 6: Run the unit tests**

Run: `cargo test --features enrich enrich` and `cargo test --features enrich token_list`/`chainlink`.
Expected: the two pure-logic tests PASS. Also confirm `cargo build` (no `enrich`) compiles nothing new, and `cargo build --features enrich` is clean.

- [ ] **Step 7: Full gate + report**, including the feature permutations:
```
cargo clippy --all-targets                       # default (no enrich)
cargo clippy --all-targets --features enrich
cargo clippy --all-targets --no-default-features --features enrich
cargo test --features enrich
```

- [ ] **Step 8: Commit (after approval)**

```bash
git add Cargo.toml Cargo.lock src/core/deps/enrich.rs src/adapters/enrich/ src/core/deps/mod.rs src/adapters/mod.rs
git commit -m "feat(enrich): TokenMetadataSource + PriceSource seam (token-list, Chainlink) behind feature enrich"
```

---

### Task 6: Docs + surface polish

**Files:**
- Modify: `README.md` (feature table / read-preview section; mark F1 done)
- Modify: `src/lib.rs` (crate-doc: mention read/preview/ens surface if the doc enumerates capabilities)

**Interfaces:** none new — this task only surfaces what Tasks 1–5 built.

- [ ] **Step 1: Update README**

Add a short "Reading & previewing" subsection: `ReadClient` (balances/metadata/allowance, Multicall3-batched), `Wallet::dry_run` → `TxPreview`, `EnsResolver`, and the opt-in `enrich` feature (token-list metadata + Chainlink prices). One example: build a `Transport`, `RpcReadClient::new(transport.provider())`, `read.balances(account, &[usdc, dai])`. Mark F1 in the status table.

- [ ] **Step 2: Doctest/compile check**

If the README example is a Rust doctest, `cargo test --doc`. Otherwise `cargo build --examples` if an example file was added. Keep examples minimal and real.

- [ ] **Step 3: Full gate + report.**

- [ ] **Step 4: Commit (after approval)**

```bash
git add README.md src/lib.rs
git commit -m "docs(read-preview): document ReadClient, dry_run, ENS, and the enrich seam"
```

---

## Self-review

**Spec coverage** (against `2026-08-24-read-preview-design.md`):
- ✅ `ReadClient` (native/ERC-20/721/1155 known-contract reads + Multicall3 `aggregate3` `balances` with per-token `Result`) → Task 1.
- ✅ `TxPreview`/`dry_run` (eth_call + estimate_gas + create_access_list; decoded `RevertReason`; revert-is-not-error; gas advisory) → Tasks 2–3.
- ✅ `EnsResolver` (4 verbs; reverse forward-verify; CCIP not followed = strict RPC) → Task 4, via `alloy-ens` (registry-based; a **deliberate reuse deviation** from the spec's "Universal Resolver" wording — same 4-verb surface, no hand-rolled namehash/encoding; note in the PR).
- ✅ Enrichment seam (`TokenMetadataSource`←token-list, `PriceSource`←Chainlink, feature `enrich`, vendor adapters deferred) → Task 5.
- ✅ Reads standalone; `ReadClient`/ENS reuse `Transport`'s resilient provider (`Transport::provider()`) → Tasks 1, 4.
- ✅ One public error type: `WalletKitError::{Read, Ens}` + `kind()`; preview reuses `Rpc` (no gratuitous variant) → Tasks 1, 3, 4.
- ✅ Deferred items (token/NFT discovery, asset-delta preview, vendor adapters, traces) are **not** implemented and the ports are shaped to admit them later.

**Placeholder scan:** no TBD/TODO; every code step carries real code; the mock bytecode is the actual compiled artifact; alloy APIs (`multicall`/`aggregate3`, `call`/`create_access_list`, `as_revert_data`, `ProviderEnsExt`, `Revert`/`Panic`/`SELECTOR`, `abi_encode`/`abi_decode_returns`) are verified against the resolved 2.4.1 / 1.6.1 sources in the registry.

**Type consistency:** `ReadClient`/`Erc20Metadata`/`AccountBalances`/`TokenBalance`/`ReadError` are defined in Task 1 and consumed by Task 5's fallback; `Simulated` (Task 2) is consumed by `dry_run` (Task 3); `TxPreview`/`SimOutcome`/`RevertReason` names are stable across Task 3; `Transport::provider()` (Task 1) is reused by Tasks 4–5. `decode_revert` free fn name matches its test.

**Research-pass deltas folded in (2026-08-24, "lean + minimal reads" scope):**
- **Correctness (non-optional):** bytes32 metadata fallback (`decode_str`); `balances` chunking + result-length guard + overridable Multicall3; Chainlink per-feed staleness (`FeedConfig.heartbeat_secs` + grace) + round validation (`EnrichError::Feed`); ENS name-normalization note + typed `EnsError::OffchainLookupRequired`; `#[non_exhaustive]` on returned structs/enums.
- **Minimal reads added:** `ReadClient::{chain_id, code, is_contract}`.
- **Deferred (behind `#[non_exhaustive]`/seams, per YAGNI):** preview `state_overrides`/`pending`/`block`; fee/total-fee reads; local ABI `decode_call`; NFT metadata URIs; unlimited-approval detector; EIP-1271/6492 + `predict_address`/HD (→F2); `dry_run_many` bundles; asset-delta/log/trace preview; vendor adapters; multi-source prices/TWAP/Pyth; avatar-NFT resolution; token-list refresh; CCIP-Read.

**Open verifications flagged inline for the implementer** (compiler-checkable, low-risk): exact `Panic` field name; `AlloyEnsError` variant names + `is_offchain_lookup` detection; `TransactionRequest::with_deploy_code`; `I256`/`U256::try_from`; `get_code_at`/`get_chain_id` method names on `DynProvider`. Each has a stated fallback.
