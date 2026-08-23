# Changelog

All notable changes to walletkit are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor = breaking).

## [Unreleased]

### Added
- Phase 1 — EVM Execution Core: `TxIntent` → policy → nonce → gas → signer →
  submission → state, wired through the `Wallet` facade.
- Executor: functional-core/imperative-shell lifecycle FSM (hash-anchored,
  finality-gated), stable `TxHandle` across RBF bumps, reorg-aware recovery.
- Localnet integration harness (embedded anvil) and executor test-hardening suite
  (77 unit + 8 localnet tests).
- Repository maintenance: dual MIT/Apache-2.0 license, CI (fmt/clippy/test),
  MSRV pin, contributor/security docs, issue/PR templates.

[Unreleased]: https://github.com/21r21a33333/walletkit/commits/main
