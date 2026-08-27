# Changelog

All notable changes to walletkit are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0: minor = breaking).

## [Unreleased]

## [0.1.0] - 2026-08-27

First release. The complete Phase-1 EVM execution core.

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
- **HD account management** (#9): `AccountManager` — BIP-39 seed generate/restore
  (fail-closed CSPRNG, zeroized, redacted), multi-account BIP-44/Ledger-Live derivation,
  watch-only account xpubs + keyless `derive_address`, counterfactual `predict_address`
  (CREATE2 + ERC-4337/6492 deploy data, Safe salt helper), gap-limit account discovery
  (batched `Rpc::account_activity` — one JSON-RPC round-trip per window, multi-chain union),
  account labels, and encrypted keystore export (`LocalSigner::export_keystore`).
- **Ergonomics / DX** (#10): `Wallet::connect_http` (one-line construction, policy explicit)
  and `connect_http_dev` (loud allow-all dev helper); `TxIntent::transfer`/`call`
  constructors; a curated `prelude` plus `types`/`units` alloy re-exports (no direct alloy
  dependency needed); `PolicyEngine::validate` → token-free `PolicyOutcome` and
  `Wallet::validate` (the policy analog of `dry_run`, bypass-proof by construction); `Debug`
  on `TxPreview`; runnable `examples/` and a compiled quickstart doctest.
- **Repository maintenance** (#2): dual MIT/Apache-2.0 license, CI (fmt/clippy/test),
  MSRV pin, contributor/security docs, issue/PR templates.
- **Supply-chain & release hardening** (#11): `#![forbid(unsafe_code)]` and a
  `deny(clippy::unwrap_used/expect_used/panic)` no-panic policy (relaxed under `cfg(test)`);
  broken/private intra-doc links denied crate-wide. New report-only `supply-chain` workflow
  runs `cargo-deny` (advisories · licenses · bans · sources) on PRs and weekly; CI gains an
  MSRV job, a `cargo-hack` feature-matrix job, a docs build, and `--locked` throughout.
  `docs.rs` metadata builds all-feature docs. Publishing prep: crates.io package name
  `walletkit-rs` with `[lib] name = "walletkit"` (import path unchanged), broader
  `categories`, and a `documentation` link.
- **Documentation pass** (#11): every public item is now documented, enforced by a
  `#![deny(missing_docs)]` gate; the crate root gains a feature-flag table and design
  pointer. New maintenance gates keep it that way — CI requires a `CHANGELOG.md` entry for
  any `src/` change (skippable via a `skip-changelog` label), and the documentation
  conventions are recorded in `CONTRIBUTING.md`.

### Changed
- **crates.io package name is `walletkit-rs`** (the `walletkit` name is taken); the library
  name stays `walletkit`, so `use walletkit::…` is unchanged for dependents.
- **MSRV corrected to 1.94.1** — the previously declared `1.85` never built; alloy 2.4.1's
  rolling MSRV is the real floor. Now pinned and CI-verified.

### Fixed
- Five rustdoc warnings: intra-doc links to private modules (`wasm`, `build`, `primitives`,
  cfg-gated `redb`/`postgres`) demoted to code spans, and `pending_handles` linked via `Self::`.

[Unreleased]: https://github.com/21r21a33333/walletkit/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/21r21a33333/walletkit/releases/tag/v0.1.0
