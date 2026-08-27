# walletkit — working agreement

Rules that override default behavior. Read **Core rules** first; the **Collaboration
workflow** governs how we go from idea to merged PR; the remaining sections are the
engineering rules for the code itself.

## Core rules (non-negotiable)

- **IMPORTANT: never commit until I have reviewed and approved** — code, docs, and plans
  alike. "Consistency" or "it's obvious" is not approval. **Confirm before every `push`.**
  **No `Co-Authored-By` trailer** on commits.
- **IMPORTANT: no `unwrap()`/`expect()` in production code.** Propagate with `?`; allowed
  only in `#[cfg(test)]`, `const`, or a documented genuinely-infallible invariant (the
  `expect` message states it).
- **IMPORTANT: redaction is mandatory on key paths** (see Observability) — no secret ever
  reaches a log, error, span, or `Debug`.
- One task at a time: write the whole task, run the gate, report the **real** output, stop
  **uncommitted**.
- Gate before stopping: `cargo fmt --check` · `cargo clippy --all-targets` (zero warnings) ·
  `cargo test`. Green **with and without** `--no-default-features`.
- Every public fallible API returns `WalletKitError` — never a raw port error or
  `Result<_, String>`.
- Update `CHANGELOG.md` `[Unreleased]` before opening any PR.
- Every test earns its place — test only logic that can regress.

## Collaboration workflow

**Per sub-project (idea → merge):**

1. **Brainstorm the design first** — never jump to code. For anything non-trivial, run a
   **research pass**: survey how leading libraries/services solve it, find the
   industry-standard API, cite sources, and let it shape the design.
2. **Write the design spec** to `docs/plans/<slice>/YYYY-MM-DD-<topic>-design.md`.
   **I review it** before planning.
3. **Write a task-by-task plan** (`-plan.md`) where each task builds **one complete
   component**; name the library primitive each non-trivial step will reuse. **I review the
   plan** before implementation.
4. **Implement task-by-task** (see below). One branch `feat/<slice>`, one PR per
   sub-project. Branch off `main` (direct pushes to `main` are disabled).
5. **Phase-close pass** — after the last task, run a CLAUDE.md-standards refactor + review
   over the whole slice (a fresh reviewer, for correctness **and** house rules); apply the
   cleanups.
6. **Open the PR** (CHANGELOG updated first). **Merge only when I say so** — merge commit +
   `--delete-branch`, matching the existing convention; then sync `main` and record status.

**Per task:**

1. Implement one plan task; write the **entire** task.
2. Run the gate and report the **real** output (never claim green without showing it). For
   each test added, give the exact single-test re-run command
   (`cargo test --lib <path>` / `cargo test --test <bin> <name>`).
3. **Teach** (my learning preference): write a deep, self-contained article on the concepts
   the task touched — the exception to normal terseness; I'm mastering these topics.
4. Stop **uncommitted**. Commit only on explicit approval, then start the next task.

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
  (A deliberate *public API surface* — e.g. a re-export module or a spec-mandated method — is
  not "unused"; its consumer is the downstream caller.)
- **Reuse before hand-rolling** — if a library fn, a built-in/default, or a solid crate does
  the job, use it and delete what the library already provides (alloy/serde/sqlx/redb/std).
  Decide the primitive at **plan-writing** time, not during implementation.
- **Named returns, not positional tuples** — a fn returning >1 typed value returns a named
  struct; call sites read `.field`, never `.0`/`.1`. (Exceptions: same-type pairs, `(K, V)`.)
- Prefer `match`/combinators over nested `if/else`; extract shared logic at the second use
  (DRY). Prefer `parking_lot` locks (no poisoning). `#[non_exhaustive]` on returned
  structs/enums so they can grow without breaking callers.
- **Encode security invariants in the type system** where possible — make the unsafe path
  *unrepresentable* (e.g. a dry-run type that structurally cannot carry a capability), not
  merely discouraged.

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
- **Redaction is mandatory on key paths.** Any fn touching a key, `PolicyApproval`, a tx to
  sign, or signed bytes is `#[instrument(skip_all, fields(<safe allow-list>))]` — allow-list,
  never deny-list. No secret ever becomes a span/event field; secret-bearing types get a
  redacting `Debug`. Keep the redaction test green.

## Tests — every test earns its place

- **No tests** for config parsing, serde derives, struct init, route registration, trivial glue.
- **Test only** logic that can regress: orchestration, error/edge paths, SQL correctness,
  non-trivial computation.
- In-memory fakes for core unit tests; a live dependency (env-gated) for adapter integration
  tests. Prefer **hermetic** harnesses (embedded anvil, cheat-codes) over external endpoints;
  when asserting real-chain values, pin a fork block and compute expected values (e.g. `cast`),
  never approximate.

## After each task — checklist

- [ ] Slice layout respected; ports one-per-file with `{TraitName}Error`.
- [ ] YAGNI (no unused types/fields/deps); reuse check (nothing hand-rolled that a library provides).
- [ ] Comments why-not-what; naming matches house style.
- [ ] Only regression-worthy tests added; each with its single-test re-run command reported.
- [ ] Public failures return `WalletKitError` (classified via `kind()`); key paths instrumented
      `skip_all`, no secrets in telemetry; green with and without `--no-default-features`.
- [ ] Gate run, real output reported. Learning article written. Left uncommitted; commit only on approval.
- [ ] At sub-project close: phase-close review pass done; `CHANGELOG.md` `[Unreleased]` updated before the PR.
