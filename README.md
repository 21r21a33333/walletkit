# walletkit

A Rust wallet-infrastructure library: one ergonomic facade over [alloy](https://alloy.rs) for sending EVM transactions safely — keys that never leave a swappable backend, un-bypassable policy guardrails, and a transaction lifecycle that survives stuck txs and reorgs.

It is a **client-side facade, not a custody service**: it integrates MPC/TEE signers, relayers, bundlers, and paymasters as pluggable backends behind narrow traits rather than operating any of them.

See [SPEC.md](SPEC.md) for the full design specification: architecture, the 7-phase roadmap, locked decisions, and cross-cutting invariants.

## Install

```sh
cargo add walletkit-rs
```

The crates.io package is `walletkit-rs`; the library imports as `walletkit`:

```rust
use walletkit::prelude::*;
```

## Quickstart

`use walletkit::prelude::*;` brings the facade, the port traits, and the common alloy
value/unit types into scope. `Wallet::connect_http` wires the transport, signer, and policy
in one call — the policy stays explicit (the guardrail is never defaulted away):

```rust,no_run
use std::sync::Arc;
use walletkit::prelude::*;
use walletkit::adapters::{LocalSigner, SystemClock};
use walletkit::adapters::policy::{DefaultPolicyEngine, TargetAllowlist};

let to = Address::from([0x22; 20]);
let signer = LocalSigner::from_private_key("0x59c6…")?;
// The recipient is the only allowed target.
let policy = DefaultPolicyEngine::new(vec![Box::new(TargetAllowlist::new([to]))], Arc::new(SystemClock));
let wallet = Wallet::connect_http("http://localhost:8545", signer, policy)?;

let intent = TxIntent::transfer(1, wallet.account(), to, parse_ether("0.01")?);
let handle = wallet.send(&intent).await?;
```

Runnable end-to-end examples live in [`examples/`](examples): `cargo run --example send_eth`,
`read_balance`, `resolve_ens`, `hd_accounts`, `preview_and_validate`. For dev/tests,
`Wallet::connect_http_dev(url, signer)` skips the policy with a loud, allow-all default.
Preview before sending: `wallet.dry_run(&intent)` (what happens on-chain) and
`wallet.validate(&intent)` (would the policy allow it).

## Private submission (MEV protection)

Route the **same signed intent** through a private relay instead of the public mempool —
same intent, same policy, same hash, no front-running. Supply the endpoint-auth identity
(a rotatable key, distinct from the tx key), then choose a route per send:

```rust
let wallet = Wallet::builder(rpc, signer, policy)
    .relay_identity(identity)   // enables private routing
    .build();

// Flashbots — the inclusion knobs exist only on this route type:
wallet.send_with(&intent, Flashbots::new(Escalation::StayPrivate).fast().within(25)).await?;

// A generic Protect relay — no knobs to misuse:
wallet.send_with(&intent, Protect::mev_blocker(Escalation::StayPrivate)).await?;
```

Routes are **type-state**: `Flashbots` carries the `eth_sendPrivateTransaction` knobs
(`within(n)`/`fast()`/`reveal(hints)`); `Protect` (`mev_blocker`/`bloxroute`/`custom`) is a
plain Protect RPC and *cannot* carry them — an invalid combination doesn't compile. The chosen
route is persisted, so bumps and crash-recovery re-broadcast **privately** — a private tx never
silently leaks to the public mempool. `Escalation::PublicAfter { cycles }` opts into a loud,
persisted fallback to public if a tx won't land; `StayPrivate` keeps retrying privately.
Without a relay identity, a private send fails cleanly (before signing) rather than leaking.

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

## Accounts (HD keys, discovery, prediction)

`AccountManager` is the seed-owning HD factory. It generates or restores a BIP-39 mnemonic
(fail-closed OS CSPRNG, zeroized in memory, redacted from logs) and derives accounts under
it. Derived signers plug straight into `Wallet::builder` — no facade change.

- **Derivation** — `account(index)` / `account_at_path(path)` and `signer(index)` under a
  selectable `PathScheme` (`Bip44Standard` `m/44'/60'/0'/0/{i}` vs Ledger Live
  `m/44'/60'/{i}'/0/0`), with an optional BIP-39 passphrase.
- **Watch-only** — `account_xpub(account)` hands out an account-level xpub; `derive_address`
  derives receive addresses from it **without the seed**.
- **`predict_address`** — counterfactual CREATE2 smart-account address + ERC-4337/6492 deploy
  data (`predict_address_checked` also reports whether it's deployed); `safe_salt` helper.
- **Discovery** — `discover(&[rpc], opts)` scans the seed with a BIP-44 gap limit, probing
  each window in **one JSON-RPC batch per chain** (`Rpc::account_activity`) and unioning
  "used" across chains; results flag `partial` / `hit_max_index`.
- **Backup** — `LocalSigner::export_keystore` writes an encrypted EIP-2335 / Web3 Secret
  Storage keystore (load it back with `from_keystore`). No raw-mnemonic export.

```rust,ignore
use walletkit::adapters::AccountManager;
use walletkit::core::accounts::WordCount;

let manager = AccountManager::generate(WordCount::W24)?; // fresh seed, retained for backup
let signer = manager.signer(0)?;                          // m/44'/60'/0'/0/0
let wallet = Wallet::builder(rpc, std::sync::Arc::new(signer), policy).build();
```

## Status

Pre-1.0 and evolving (minor releases may break, per the [changelog](CHANGELOG.md)). The
Phase-1 EVM execution core is complete: transaction execution with policy-gated signing,
durable state and crash recovery, a resilient RPC transport, reads/preview/ENS/pricing,
and HD account management. See [SPEC.md](SPEC.md) for the full roadmap.
