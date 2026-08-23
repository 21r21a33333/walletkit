# walletkit

A Rust wallet-infrastructure library: one ergonomic facade over [alloy](https://alloy.rs) for sending EVM transactions safely — keys that never leave a swappable backend, un-bypassable policy guardrails, and a transaction lifecycle that survives stuck txs and reorgs.

It is a **client-side facade, not a custody service**: it integrates MPC/TEE signers, relayers, bundlers, and paymasters as pluggable backends behind narrow traits rather than operating any of them.

See [SPEC.md](SPEC.md) for the full design specification: architecture, the 7-phase roadmap, locked decisions, and cross-cutting invariants.

## RPC layer — eRPC recommended

walletkit's `Transport` reuses alloy's transport layers (retry/backoff + multi-endpoint
failover) and adds no bespoke resilience. For production, we **recommend running
[eRPC](https://github.com/erpc/erpc)** as your RPC layer and pointing walletkit at it
with `Transport::single(erpc_url)`. eRPC owns the RPC-management catalog — failover,
hedging, reorg-aware caching, request dedup, cross-upstream quorum, rate-limits, and
per-method overrides — so walletkit stays thin and you configure RPC policy in one place.

Without eRPC, `Transport::new(primary, fallbacks)` gives in-process failover across
multiple endpoints.

## Status

Design locked; Phase 1 (EVM Execution Core) implementation in progress.
