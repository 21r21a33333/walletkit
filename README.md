# walletkit

A Rust wallet-infrastructure library: one ergonomic facade over [alloy](https://alloy.rs) for sending EVM transactions safely — keys that never leave a swappable backend, un-bypassable policy guardrails, and a transaction lifecycle that survives stuck txs and reorgs.

It is a **client-side facade, not a custody service**: it integrates MPC/TEE signers, relayers, bundlers, and paymasters as pluggable backends behind narrow traits rather than operating any of them.

See [SPEC.md](SPEC.md) for the full design specification: architecture, the 7-phase roadmap, locked decisions, and cross-cutting invariants.

## RPC layer — eRPC recommended

walletkit's `Transport` reuses alloy's transport layers (retry/backoff + multi-endpoint
failover) and adds no bespoke resilience. For production, we **recommend running
[eRPC](https://github.com/erpc/erpc)** as your RPC layer and pointing walletkit at it
with `Transport::url(erpc_url)`. eRPC owns the RPC-management catalog — failover,
hedging, reorg-aware caching, request dedup, cross-upstream quorum, rate-limits, and
per-method overrides — so walletkit stays thin and you configure RPC policy in one place.

Without eRPC, `Transport::builder(primary).fallbacks(rest).build()` (or
`Transport::from_config`) gives in-process failover across multiple endpoints.

## Reading, previewing & names

Beyond sending, walletkit exposes read-only surfaces over the same resilient `Transport`,
so reads inherit its failover/retry:

- **`ReadClient`** — `chain_id` / `code` / `is_contract`, native balance, ERC-20
  (balance/allowance/metadata), and ERC-721/1155 reads for known contracts.
  `balances(account, &[tokens])` folds the native balance and each token's `balanceOf` into
  **one Multicall3 round-trip**, with a per-token `Result` so one non-conforming token can't
  fail the scan. Metadata tolerates `bytes32` name/symbol (MKR/DAI).
- **`Wallet::dry_run(&intent)` → `TxPreview`** — an RPC-only pre-sign simulation: gas
  (advisory), success or a **decoded revert reason**, EIP-2930 access list, and return data.
  A would-revert tx is a *successful* preview with a `Revert` outcome, not an error.
- **`EnsResolver`** — `resolve_name` / `reverse_lookup` (forward-verified) / `text_record` /
  `avatar`. Offchain (CCIP-Read) names surface as a typed error rather than being followed.
- **`pricing` feature (opt-in)** — `TokenMetadataSource` (Uniswap token-list) and
  `PriceSource` (Chainlink, per-feed staleness); core stays vendor-free. Enable with
  `features = ["pricing"]`.

```rust,ignore
use walletkit::adapters::{RpcReadClient, Transport};
use walletkit::core::deps::ReadClient;

let transport = Transport::url("http://localhost:8545".parse()?)?;
let read = RpcReadClient::new(transport.provider());

// One Multicall3 round-trip: native + each token, failure-isolated per token.
let overview = read.balances(account, &[usdc, dai]).await?;
println!("ETH {} · USDC {:?}", overview.native, overview.tokens[0].balance);
```

Reads and ENS are **standalone** (built from a `Transport`, targeting any address);
`Wallet::dry_run` is the one wallet-bound convenience.

## Status

Design locked; Phase 1 (EVM Execution Core) implementation in progress — the read /
preview / ENS / pricing DX seams (sub-project F1) are implemented.
