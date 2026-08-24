# walletkit — working agreement

Rules that override default behavior. Read **Core rules** first; the sections below expand them.

## Core rules (non-negotiable)

- **IMPORTANT: never commit until I have reviewed and approved** — code, docs, and plans
  alike. "Consistency" or "it's obvious" is not approval. **Confirm before every `push`.**
- **IMPORTANT: no `unwrap()`/`expect()` in production code.** Propagate with `?`; allowed
  only in `#[cfg(test)]`, `const`, or a documented genuinely-infallible invariant (the
  `expect` message must state it).
- One task at a time: write the whole task, run the gate, report the **real** output, stop
  **uncommitted**.
- Gate before stopping: `cargo fmt --check` · `cargo clippy --all-targets` (zero warnings) ·
  `cargo test`. Must stay green **with and without** `--no-default-features`.
- Every public fallible API returns `WalletKitError` — never a raw port error or
  `Result<_, String>`.
- Update `CHANGELOG.md` `[Unreleased]` before opening any PR.
- Every test earns its place — test only logic that can regress.

## Workflow — task-by-task, review-gated

1. Implement one plan task (`docs/plans/`); write the entire task.
2. Run the gate; report the real output (never claim green without showing it).
3. Stop uncommitted. Commit only on explicit approval, then move to the next task.
4. Before opening a PR, record the sub-project's user-facing changes in `CHANGELOG.md`
   `[Unreleased]` under the right heading (Added/Changed/Fixed), citing the PR number.

## Architecture — single crate, sliced by domain

- `core/<slice>/{primitives,service}.rs` — domain types + orchestration, **zero I/O**.
- `core/deps/<slice>/*.rs` — ports: `#[async_trait]` behind `Arc<dyn Trait>`, **one file per
  port**, each with its own `thiserror` enum `{TraitName}Error`.
- `adapters/<slice>/…` — port implementations (DB/HTTP/signer/RPC) + inbound handlers.
- Composition root = the facade (walletkit is a library, not a `main.rs`).
- Prefer **fewer, larger, focused files** over many tiny ones.

## Code style — house rules (only what differs from defaults)

- **Comments say why, not what** — short, minimal; most lines need none. No comment-per-change,
  no dev-process breadcrumbs (task/step numbers, "grows in Phase N", "was/refactored"). Spec
  anchors (`EIP-2`, `§5.2`) are fine; roadmap references are noise. Doc summaries 1–2 sentences.
- **Naming:** accessors drop `get_` (bare noun), writes are domain verbs, predicates are
  `is_`/`supports_`. Fix outliers to match.
- **YAGNI** — add a type/field/variant/dep only at its first real consumer; don't pre-build.
- **Reuse before hand-rolling** — if a library fn, a built-in/default, or a solid crate does
  the job, use it and delete what the library already provides (alloy/serde/sqlx/redb/std).
  Decide the primitive at **plan-writing** time, not during implementation.
- **Named returns, not positional tuples** — a fn returning >1 typed value returns a named
  struct; call sites read `.field`, never `.0`/`.1`. (Exceptions: same-type pairs, `(K, V)`.)
- Prefer `match`/combinators over nested `if/else`; extract shared logic at the second use
  (DRY). Prefer `parking_lot` locks (no poisoning).

## Observability & errors

- One public error type (`WalletKitError`); per-port `{Trait}Error`s map in via `From`. A new
  failure mode must (a) surface as a `WalletKitError` variant and (b) be classified in `kind()`
  (`Retryable`/`Terminal`/`NeedsReconcile`); add a `remediation()` hint when it helps.
- Instrument via the shim (`use crate::obs::{info, warn, error, debug};`), never `tracing::`
  directly; spans via `#[cfg_attr(feature = "tracing", tracing::instrument(...))]`. Never
  install a subscriber or depend on `opentelemetry` — the host owns that.
- Correlate by `intent_hash` (one span per intent). Levels: ERROR = terminal caller-facing ·
  WARN = recoverable (bump/reorg/failover/retry) · INFO = sparse milestones (~1/tx) ·
  DEBUG = mechanics · TRACE = raw. Keep INFO sparse.
- **IMPORTANT: redaction is mandatory on key paths.** Any fn touching a key, `PolicyApproval`,
  a tx to sign, or signed bytes is `#[instrument(skip_all, fields(<safe allow-list>))]` —
  allow-list, never deny-list. No secret ever becomes a span/event field; secret-bearing types
  get a redacting `Debug`. Keep the redaction test green.

## Tests — every test earns its place

- **No tests** for config parsing, serde derives, struct init, route registration, trivial glue.
- **Test only** logic that can regress: orchestration, error/edge paths, SQL correctness,
  non-trivial computation.
- In-memory fakes for core unit tests; a live dependency (gated by env var) for adapter
  integration tests.

## After each task — checklist

- [ ] Slice layout respected; ports one-per-file with `{TraitName}Error`.
- [ ] YAGNI (no unused types/fields/deps); reuse check (nothing hand-rolled that a library provides).
- [ ] Comments why-not-what; naming matches house style.
- [ ] Only regression-worthy tests added.
- [ ] Public failures return `WalletKitError` (classified via `kind()`); key paths instrumented
      `skip_all`, no secrets in telemetry; green with and without `--no-default-features`.
- [ ] Gate run, real output reported. Left uncommitted; commit only on approval.
- [ ] At sub-project close, before the PR: `CHANGELOG.md` `[Unreleased]` updated.
