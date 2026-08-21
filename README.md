# walletkit

A Rust wallet-infrastructure library: one ergonomic facade over [alloy](https://alloy.rs) for sending EVM transactions safely — keys that never leave a swappable backend, un-bypassable policy guardrails, and a transaction lifecycle that survives stuck txs and reorgs.

It is a **client-side facade, not a custody service**: it integrates MPC/TEE signers, relayers, bundlers, and paymasters as pluggable backends behind narrow traits rather than operating any of them.

See [SPEC.md](SPEC.md) for the full design specification: architecture, the 7-phase roadmap, locked decisions, and cross-cutting invariants.

## Status

Design locked; Phase 1 (EVM Execution Core) implementation in progress.
