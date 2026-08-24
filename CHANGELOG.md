# Changelog

All notable changes to walletkit are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor = breaking).

## [Unreleased]

### Added
- **EVM execution core** (#1): `TxIntent` → policy → nonce → gas → signer →
  submission → state, wired through the `Wallet` facade. Functional-core/imperative-shell
  lifecycle FSM (hash-anchored, finality-gated), stable `TxHandle` across RBF bumps,
  reorg-aware recovery. Localnet integration harness (embedded anvil).
- **Error taxonomy & observability** (#3): one public `WalletKitError` classified by
  `kind()` (`Retryable` / `Terminal` / `NeedsReconcile`) with `remediation()` hints;
  per-port `{Trait}Error` contracts mapped in via `From`. Optional `tracing`
  instrumentation behind a feature flag, intent-hash correlation, and mandatory
  redaction on key paths — builds green with and without `--no-default-features`.
- **Durable state & recovery** (#4): `StateStore` port with redb (embedded default),
  Postgres, and in-memory backends; crash-recovery reconciliation; nonce
  ownership/fencing seam (single-writer-per-account default).
- **Signing surface** (#5): EIP-191 personal-sign and EIP-712 typed-data signing,
  policy-gated (blind-sign default-deny), low-s normalization.
- **Lifecycle completeness** (#6): transaction cancellation via self-send replacement,
  `TxStatus::Dropped`, and opt-in intent-refill on terminal drop.
- **Reads, preview & names** (#7): `ReadClient` — native/ERC-20/721/1155 balances &
  metadata, `chain_id`/`code`/`is_contract`, and Multicall3 `aggregate3` batching with
  per-token failure isolation. `Wallet::dry_run` → `TxPreview` (eth_call + estimate_gas +
  create_access_list, decoded `RevertReason`; a revert is not an error). `EnsResolver`
  (forward-verified reverse). Opt-in `pricing` feature: token-list metadata + Chainlink
  price with per-feed staleness.
- **Repository maintenance** (#2): dual MIT/Apache-2.0 license, CI (fmt/clippy/test),
  MSRV pin, contributor/security docs, issue/PR templates.

[Unreleased]: https://github.com/21r21a33333/walletkit/commits/main
