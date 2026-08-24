# Sub-project F1 — Read & Preview (design)

**Status:** brainstormed 2026-08-24 · **Branch:** `feat/read-preview` · **Phase:** 1 DX seams (F1 of F = read-preview → account-manager → ergonomics) · **Depends on:** 0/A/B/C/D on `main`; reuses the `Rpc`/`Transport` port. **Research-cited** (3-agent survey of viem/ethers/alloy/wagmi, Tenderly/Alchemy/Blocknative/JSON-RPC, Alchemy/thirdweb/Circle/Safe/Fireblocks/OZ + token-lists/Chainlink/ENS).

## Goal

Give consumers a read-only **view** of chain state and a pre-sign **simulation** — the surface every wallet UI needs — without leaving the RPC-only, object-safe character of the library.

**Organizing principle (research-derived):** *a read is core iff the contract address is already known.* The moment "**which** tokens/NFTs does this address hold?" is itself the question, it is discovery → an indexer concern, and stays a deferred seam. This one rule splits F1 cleanly into RPC-only core vs. opt-in seam, matching how viem, alloy, Alchemy, thirdweb, and Safe all draw the line.

## Scope

**In — core, RPC-only over the existing `Rpc`/`Transport`, `sol!` for ABIs:**

- **`ReadClient`** — `chain_id`, `code`/`is_contract` (EOA-vs-contract, deployed check); native balance; ERC-20 balance/allowance/metadata; ERC-721/1155 ownership + balance for **known** contracts; a batched `balances(account, tokens)` (native + per-token) over **Multicall3 `aggregate3`** (per-call `Result`, so one reverting token can't nuke a portfolio scan). Modeled on viem's `PublicClient`. Metadata reads fall back `string`→`bytes32` for non-standard tokens (MKR/DAI); `balances` chunks large lists to stay under node calldata/gas caps and takes an overridable Multicall3 address.
- **`dry_run` → `TxPreview`** — gas estimate, success/revert with a **decoded reason**, EIP-2930 access list, raw return data. RPC-only via `eth_call` + `eth_estimateGas` + `eth_createAccessList` (alloy exposes `create_access_list` natively; viem/ethers don't). Gas is **advisory** — a dry-run is a lower bound.
- **`EnsResolver`** — resolve / reverse / text / avatar over the ENS Universal Resolver (`eth_call`).
- **Enrichment (opt-in ports; the RPC-compatible adapters ship, so core carries no vendor dependency):** `TokenMetadataSource` ← a Uniswap **token-list** adapter; `PriceSource` ← a **Chainlink** `latestRoundData` adapter. Feature `enrich`.

**Out — deferred, but designed to slot into the same seams:**

| Deferred item | Lands in | Why |
| --- | --- | --- |
| Token/NFT **discovery** & enumeration ("which tokens does X hold") | an indexer sub-project | No `getAllTokensOf` exists on-chain; needs Transfer-log scanning or a vendor index (Alchemy `getTokenBalances`, thirdweb Insight). |
| USD-priced asset/balance **deltas** in preview | the `AssetPreview` seam | Needs `eth_simulateV1 traceTransfers` (not universally supported — Tenderly's node rejects it) or a vendor (Tenderly/Alchemy `simulateAssetChanges`). |
| **Vendor** adapters (Alchemy / Tenderly / CoinGecko) | with a real consumer | The `PriceSource`/metadata/preview ports exist; a vendor adapter pulls HTTP deps + a key, so it lands when someone needs it (YAGNI). |
| Decoded internal call **traces** / state diffs in preview | provider seam | `debug_trace*` is commonly disabled on hosted nodes; provider-specific. |

## Architecture

Three small object-safe read ports + concrete adapters, plus one opt-in enrichment seam. Every unit here is **read-only** — no signer, no `PolicyApproval`, no state mutation — so none of it touches the send/executor path. Isolation per the hexagonal `evm-executor` convention (core/deps ports, flat adapters).

- `core/deps/read.rs` — `ReadClient` trait + `ReadError`.
- `adapters/read.rs` — `RpcReadClient` over the **same resilient alloy provider `Transport` builds** (so reads inherit its failover/retry/hedge; `Transport` exposes a cheap `DynProvider` clone or a `read_client()` constructor); `sol!` ERC-20/721/1155 bindings + `MulticallBuilder`.
- `core/wallet/preview.rs` — the `TxPreview` type + `dry_run` orchestration; `Rpc` gains `call` + `create_access_list`; `Transport` implements them.
- `core/deps/ens.rs` — `EnsResolver` trait + `EnsError`; `adapters/ens.rs` — Universal-Resolver adapter.
- `core/deps/enrich.rs` — `TokenMetadataSource`, `PriceSource` traits + errors; `adapters/enrich/{token_list,chainlink}.rs` behind feature `enrich`.
- `facade.rs` — construction: `ReadClient`/`EnsResolver` are standalone (built from a `Transport`, **not** tied to the wallet's single account, since reads target arbitrary addresses); `Wallet::dry_run(&intent)` is a convenience over the wallet's chain.

## Components

### `ReadClient` (port + `RpcReadClient` adapter)

Object-safe (`#[async_trait]`, `Arc<dyn ReadClient>`); every method returns a concrete domain type — `sol!` types stay inside the adapter, never on the port boundary (that would break object-safety), per the alloy takeaway.

```rust
#[async_trait]
pub trait ReadClient: Send + Sync {
    async fn chain_id(&self) -> Result<u64, ReadError>;
    /// `eth_getCode`; `is_contract` is `!code.is_empty()` — EOA-vs-contract branching and
    /// the substrate for later EIP-1271-vs-ECDSA signature dispatch.
    async fn code(&self, address: Address) -> Result<Bytes, ReadError>;
    async fn is_contract(&self, address: Address) -> Result<bool, ReadError>;
    async fn native_balance(&self, account: Address) -> Result<U256, ReadError>;
    async fn erc20_balance(&self, token: Address, account: Address) -> Result<U256, ReadError>;
    async fn erc20_allowance(&self, token: Address, owner: Address, spender: Address) -> Result<U256, ReadError>;
    async fn erc20_metadata(&self, token: Address) -> Result<Erc20Metadata, ReadError>;
    async fn erc721_owner_of(&self, token: Address, token_id: U256) -> Result<Address, ReadError>;
    async fn erc721_balance(&self, token: Address, account: Address) -> Result<U256, ReadError>;
    async fn erc1155_balance(&self, token: Address, account: Address, id: U256) -> Result<U256, ReadError>;
    /// One Multicall3 `aggregate3`: native + each token's `balanceOf`, per-token `Result` so a
    /// single reverting token doesn't fail the overview.
    async fn balances(&self, account: Address, tokens: &[Address]) -> Result<AccountBalances, ReadError>;
}

// All returned structs/enums are `#[non_exhaustive]` so fields (e.g. a metadata `source`
// or `logo_uri`, a price `confidence`) can grow later without a breaking change — YAGNI:
// add the field when a consumer needs it, don't pre-commit the shape now.
#[non_exhaustive]
pub struct Erc20Metadata { pub name: String, pub symbol: String, pub decimals: u8 }
#[non_exhaustive]
pub struct AccountBalances { pub native: U256, pub tokens: Vec<TokenBalance> }
/// One token's balance in a batch; `Err` isolates a reverting/non-conforming token
/// (named struct, not a positional `(Address, Result<…>)` tuple).
pub struct TokenBalance { pub token: Address, pub balance: Result<U256, ReadError> }
```

Adapter internals (`RpcReadClient { provider: DynProvider }` — the provider is `Transport`'s, so reads ride the same failover/retry/hedge layers, not a fresh single endpoint):
- Typed reads via `sol!` (`IERC20`, `IERC721`, `IERC1155`) — `contract.balanceOf(a).call().await` returns the decoded value.
- `balances` uses `provider.multicall()` with `.get_eth_balance(account)` folded in and `.aggregate3()` (per-call `Result`); Multicall3 default `0xcA11…CA11`, overridable. Native balance rides *inside* the aggregate, so a "wallet overview" (ETH + N tokens) is **one** RPC round-trip.
- A `metadata` read is one `aggregate3` of `name`/`symbol`/`decimals` (three calls, one RPC). Each string field decodes `string` first, then falls back to `bytes32` (null-trimmed UTF-8) — non-standard tokens (MKR, DAI, SAI) return `bytes32` and a `string`-only decode would garble/fail (reuse the Solady `MetadataReaderLib` / Uniswap `SafeERC20Namer` pattern, don't hand-roll).
- `balances` **chunks** a large token list into multiple `aggregate3` calls (node calldata/gas caps; viem defaults ~1024 calldata bytes/chunk) and stitches results, preserving per-token `Result`; the result vector is length-checked before indexing. Multicall3 defaults to the canonical address but is overridable for chains that deploy it elsewhere.

**Reuse:** all of alloy — `sol!`, `MulticallBuilder`, `DynProvider`. No hand-rolled ABI encoding, no bespoke multicall.

### `TxPreview` (`dry_run`)

RPC-only, node-portable. Composed from three standard calls; nothing here needs a special provider.

```rust
pub struct TxPreview {
    pub gas_estimate: Option<u64>,        // eth_estimateGas — ADVISORY (dry-run is a lower bound)
    pub outcome: SimOutcome,              // eth_call status
    pub access_list: Option<AccessList>,  // eth_createAccessList (also: touched addrs/slots)
    pub return_data: Bytes,               // eth_call raw return (caller ABI-decodes)
}
pub enum SimOutcome { Success, Revert(RevertReason) }
/// Standard, RPC-only decode of `eth_call` error data — no provider needed.
pub enum RevertReason {
    Error(String),                          // 0x08c379a0 Error(string)
    Panic(u64),                             // 0x4e487b71 Panic(uint256)
    Custom { selector: [u8; 4], data: Bytes },
    Unknown(Bytes),                         // empty / opaque
}
```

`Wallet::dry_run(&intent) -> Result<TxPreview, WalletKitError>`: build the `TransactionRequest` from the intent (as `send` does), then `rpc.call` (outcome + return), `rpc.estimate_gas` (gas), `rpc.create_access_list` (access list). A revert on `call` is decoded into `RevertReason`, **not** an error — a preview of a would-revert tx is a successful preview with a `Revert` outcome. `dry_run` never signs and never mutates state.

`Rpc` port gains: `call(&self, tx: &TransactionRequest) -> Result<Bytes, RpcError>` and `create_access_list(&self, tx: &TransactionRequest) -> Result<AccessListResult, RpcError>` (both already on alloy's `Provider`).

### `EnsResolver` (port + adapter)

Plain-RPC (Universal Resolver via `eth_call`); its own small port so it's optional/mockable and doesn't bloat the read path. Mirrors viem's four verbs, no more.

```rust
#[async_trait]
pub trait EnsResolver: Send + Sync {
    async fn resolve_name(&self, name: &str) -> Result<Option<Address>, EnsError>;   // getEnsAddress
    async fn reverse_lookup(&self, addr: Address) -> Result<Option<String>, EnsError>; // getEnsName
    async fn text_record(&self, name: &str, key: &str) -> Result<Option<String>, EnsError>;
    async fn avatar(&self, name: &str) -> Result<Option<String>, EnsError> { self.text_record(name, "avatar").await }
}
```

Adapter notes baked in (correctness the underlying `alloy-ens` binding does **not** give us for free):
- **Reverse forward-verify** is mandatory and in-crate: a reverse record is user-settable/unauthenticated, so `reverse_lookup` normalizes the candidate name (ENSIP-15) then forward-resolves it and asserts it maps back to the queried address — mismatch → `None`, never the raw name (spoofing guard). `alloy-ens` `lookup_address` does not do this.
- **CCIP-Read (EIP-3668) is deferred, strict-RPC is the default.** `alloy-ens` forward resolution routes through the Universal Resolver (so ENSIP-10 wildcard is free) but does **not** follow the `OffchainLookup` revert — offchain/L2 names (Basenames `*.base.eth`, `*.cb.id`, L2 subnames) therefore do **not** resolve yet. Surface that distinctly as `EnsError::OffchainLookupRequired { .. }` (not a generic failure) so callers can detect it and opt into a future CCIP feature (an HTTP gateway hop is a new trust boundary — feature-gated, never silent).
- `avatar` returns the raw text record (`Option<String>`); typed NFT-avatar (ENSIP-12 `eip155:` + ownership check) resolution is a deferred, separately-named method to avoid a breaking return-type change.

### Enrichment seam (opt-in, feature `enrich`)

Two independent ports the core never calls; a caller composes them in. Core stays vendor-free.

```rust
#[async_trait]
pub trait TokenMetadataSource: Send + Sync {
    async fn metadata(&self, chain_id: u64, token: Address) -> Result<Option<Erc20Metadata>, EnrichError>;
}
#[async_trait]
pub trait PriceSource: Send + Sync {
    async fn price(&self, chain_id: u64, token: Address, vs: Currency) -> Result<Option<Price>, EnrichError>;
}
```

- **`TokenListSource`** (adapter) — loads a Uniswap-schema token-list JSON (HTTPS/IPFS), builds an in-memory `(chain_id, address) → Erc20Metadata` map; **no RPC at read time**. Core's on-chain `erc20_metadata` fills gaps the list misses.
- **`ChainlinkPrice`** (adapter) — `AggregatorV3Interface.latestRoundData()` normalized by the feed's own `decimals()`; **pure RPC** (needs only the feed address per pair/chain), so a "no third-party APIs" deployment still gets prices. **Staleness is per-feed, not a single global constant** — heartbeats vary widely (ETH/USD is 3600s on mainnet but 86400s on Arbitrum), so config is a `(chain_id, feed) → heartbeat` map plus a small grace, checked against an injected `Clock` (no ambient time). Every round is validated: `answer > 0`, `updatedAt != 0 && updatedAt <= now`, `roundId != 0`; deprecated `answeredInRound`/`latestAnswer` are not used. A stale feed returns `EnrichError::Stale`, a missing feed returns `Ok(None)`. CoinGecko/Pyth are the *same* `PriceSource` trait, deferred (Pyth's confidence band maps to a future `#[non_exhaustive]` `Price` field).

## Data flow

```
Wallet overview:  read.balances(account, [usdc, dai, weth])
  └─ one Multicall3 aggregate3: [get_eth_balance, usdc.balanceOf, dai.balanceOf, weth.balanceOf]
     → AccountBalances { native, [(usdc, Ok), (dai, Ok), (weth, Err(reverted))] }   # one RPC

Preview:  wallet.dry_run(&intent)
  ├─ rpc.call(req)            -> return bytes | revert data → SimOutcome (decode reason)
  ├─ rpc.estimate_gas(req)    -> gas_estimate (advisory)
  └─ rpc.create_access_list(req) -> access_list (+ touched addrs/slots)
     → TxPreview { .. }        # no signing, no state change
```

## Error handling

- One `{Trait}Error` per port (`ReadError`, `EnsError`, `EnrichError`), `thiserror`, `#[non_exhaustive]` — each wraps `RpcError` (transient/terminal already classified there) plus its own decode variants (e.g. `ReadError::Decode` for a malformed on-chain response; `EnrichError::List` for a bad token-list).
- Public read/preview methods on `Wallet` return `WalletKitError`; add `Read`/`Ens`/`Preview` variants classified in `kind()` (`RpcError` drives Retryable/Terminal). A **revert in a preview is not an error** — it's a `TxPreview` with `SimOutcome::Revert`.
- `dry_run` reuses the send pipeline's `TransactionRequest` build; no new tx mechanics.

## Testing (each earns its place)

- **`ReadClient` (localnet):** deploy a mock ERC-20 to anvil, mint to an account; assert `native_balance`, `erc20_balance/allowance/metadata`, and a `balances([token, reverting_token])` where the batch returns `Ok` + per-token `Err` (proves `aggregate3` per-call failure isolation). Matrix over backends is **not** needed — reads don't touch the store.
- **`TxPreview` (unit + localnet):** unit-decode each `RevertReason` (Error/Panic/custom-selector) from crafted error bytes; localnet `dry_run` of a transfer (Success + gas + access list) and of a would-revert call (Revert with decoded reason), asserting no state change.
- **`EnsResolver`:** reverse-lookup forward-verify (mismatch → `None`) as a unit test against a mocked resolver; a live-gated mainnet-fork resolve if cheap, else skip.
- **Enrichment:** token-list parse + lookup + on-chain fallback (unit); Chainlink decode/scale/staleness (unit with a mock feed). No test for serde structs or trait plumbing.

## Files touched

`core/deps/{read,ens,enrich}.rs` (ports + errors) · `adapters/{read,ens}.rs` + `adapters/enrich/{token_list,chainlink}.rs` · `core/wallet/preview.rs` (`TxPreview`, `dry_run`) · `core/deps/rpc.rs` + `adapters/transport/mod.rs` (`call`, `create_access_list`) · `facade.rs` (`Wallet::dry_run`, `ReadClient`/`EnsResolver` wiring) · `error.rs` (`Read`/`Ens`/`Preview` variants + `kind()`) · `Cargo.toml` (feature `enrich`).

## Prior art & research (cited)

**Read surface** — model `ReadClient` on viem's [`PublicClient`](https://viem.sh/docs/clients/public) (ethers has no read-client abstraction and no native Multicall3). Batching converges on **Multicall3 `aggregate3`/allow-failure** across [viem multicall](https://viem.sh/docs/contract/multicall), [wagmi `useReadContracts`](https://wagmi.sh/react/api/hooks/useReadContracts), and alloy's [`MulticallBuilder`](https://docs.rs/alloy/latest/alloy/providers/struct.MulticallBuilder.html) (`aggregate` all-or-nothing vs `aggregate3` per-call). Native balance folds into the multicall (`.get_eth_balance()`). `sol!` is the idiomatic typed ABI binding ([alloy Provider](https://docs.rs/alloy/latest/alloy/providers/trait.Provider.html)).

**Preview** — a minimal preview is 100% RPC-portable: [`eth_call`](https://ethereum.org/developers/docs/apis/json-rpc/) (+ standard revert-data decode: `Error(string)` `0x08c379a0`, `Panic` `0x4e487b71`, custom selectors), `eth_estimateGas`, and [`eth_createAccessList`](https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-eth) — which alloy surfaces as `create_access_list` (viem/ethers do not). Richer previews (asset/balance deltas, USD, decoded traces) require [Tenderly](https://docs.tenderly.co/simulations/asset-balance-changes) / [Alchemy `simulateAssetChanges`](https://www.alchemy.com/docs/reference/simulation-asset-changes) or `eth_simulateV1 traceTransfers` (not universal) → deferred seam. viem's [`simulateContract`](https://viem.sh/docs/contract/simulateContract) confirms decoded-return + decoded-revert over `eth_call` as the right minimal primitive. Gas from a dry-run is advisory (lower bound).

**RPC-vs-indexer boundary** — token *discovery* ([Alchemy `getTokenBalances`](https://www.alchemy.com/docs/reference/token-api-overview), [thirdweb Insight](https://portal.thirdweb.com/changelog/insight-token-owner-queries-add-balances)), NFT *enumeration* ([Alchemy `getNftsForOwner`](https://www.alchemy.com/docs/reference/nft-api-endpoints/nft-api-endpoints/nft-ownership-endpoints/get-nf-ts-for-owner-v-3)), and vendor portfolio/balances ([Circle](https://developers.circle.com/api-reference/wallets/developer-controlled-wallets/list-wallet-balance), [Fireblocks](https://developers.fireblocks.com/reference/getvaultbalancebyasset)) are inherently indexed. Known-address reads, Safe config reads, and all ENS are plain-RPC. **Enrichment** = [Uniswap token-lists](https://github.com/Uniswap/token-lists) (off-chain data) + prices via [Chainlink `AggregatorV3Interface`](https://docs.chain.link/data-feeds/using-data-feeds) (RPC) or [CoinGecko](https://docs.coingecko.com/reference/simple-token-price) (vendor). **ENS** — four verbs over the Universal Resolver via `eth_call` ([viem ENS](https://viem.sh/docs/ens/actions/getEnsAddress)); reverse forward-verifies.

## Research pass (2026-08-24) — what was folded in vs. deferred

A 4-agent survey of read/preview/enrichment surfaces across viem/ethers/alloy/wagmi, Tenderly/Alchemy/Blocknative/`eth_simulateV1`, Circle/Fireblocks/Privy/Safe/thirdweb/Alchemy-Account-Kit, and Chainlink/token-lists/ENS. Scope decision: **lean + minimal reads** (correctness plus the trivially-justified reads), everything else deferred behind `#[non_exhaustive]` per YAGNI.

- **Folded into F1 (correctness / house-rules, non-optional):** bytes32 metadata fallback; `balances` chunking + result-length guard + overridable Multicall3; per-feed Chainlink staleness + round validation; ENS name normalization + reverse forward-verify + typed `OffchainLookupRequired`; `#[non_exhaustive]` on every returned struct/enum; named `TokenBalance` (no mixed-type tuple).
- **Folded into F1 (minimal reads):** `chain_id()`, `code()`/`is_contract()`.
- **Deferred behind `#[non_exhaustive]` / seams (add when a consumer lands):** preview `state_overrides` + `dry_run` at `pending` + `block: Option<BlockId>`; fee/total-fee read surface; local ABI `decode_call`; NFT metadata URIs; unlimited-approval detector; EIP-1271/6492 verify + `predict_address`/HD (→ F2, design the `factory`/`factory_data` return fields there); `dry_run_many` bundles (`eth_simulateV1`, partial support); asset-delta/log/trace preview (`AssetPreview`); vendor adapters; multi-source prices/TWAP/Pyth; avatar-NFT resolution; token-list auto-refresh; CCIP-Read; EIP-2612/5267/4626.

## Locked decisions

1. **Core is RPC-only and vendor-free.** A read is core iff the contract address is known; discovery/enumeration/vendor-portfolio is a deferred indexer seam.
2. **Multicall3 `aggregate3` (per-call `Result`) is the batch default** — one reverting token can't fail an overview; native balance rides inside the aggregate.
3. **`TxPreview` is RPC-only** (`eth_call` + `eth_estimateGas` + `eth_createAccessList`); asset/balance **deltas**, USD, and traces are a deferred `AssetPreview` seam. Gas is advisory. A revert is a *successful preview* with a `Revert` outcome, not an error.
4. **Enrichment ships only its RPC-compatible adapters** (token-list metadata, Chainlink price) behind feature `enrich`; vendor adapters (Alchemy/Tenderly/CoinGecko) are the same ports, deferred.
5. **ENS is its own small port** (four verbs), plain-RPC; offchain CCIP-Read behind a flag.
6. **Reads are standalone**, not bound to the wallet's single account (they target arbitrary addresses); `Wallet::dry_run` is a convenience over the wallet's chain.
7. **`ReadClient` reuses `Transport`'s resilient provider** (failover/retry/hedge from sub-project 0), never a naive single endpoint — the read path gets the same reliability as the write path for free.
8. **Correctness the libraries don't hand us is in-crate and non-optional:** bytes32 metadata fallback, per-feed Chainlink staleness + round validation, and ENS reverse forward-verification. These are the sharp edges `alloy`/`alloy-ens`/Chainlink leave to the caller.
9. **`#[non_exhaustive]` on every returned struct/enum**, so deferred fields (metadata `source`/`logo_uri`, price `confidence`, …) are added when a consumer needs them — grow, don't pre-commit (YAGNI). Return-*type* changes (e.g. a typed `Avatar`) instead land as new, separately-named methods.
10. **Minimal reads only:** `chain_id`/`code`/`is_contract` ship now (EIP-712 domain + EOA-vs-contract branching); the broader confirm-screen surface (state overrides, fee/total-fee, ABI decode, NFT URIs, call bundles) is deferred until a real consumer needs it — the port/struct shapes don't preclude it.
11. **Enrichment stays in F1 but is built correctly** (per-feed staleness, bytes32 fallback); vendor price/metadata adapters remain deferred behind the same ports.
