# walletkit — working agreement

These rules override default behavior and MUST be followed for every task.

## Workflow — task-by-task, review-gated

1. Implement one plan task at a time (see `docs/plans/`).
2. Write the **entire** code for that task. **Do NOT commit.**
3. Before stopping, run the full gate and **report the real output** (never claim
   green without showing it):
   ```
   cargo fmt --check     # formatting clean
   cargo clippy --all-targets   # zero warnings
   cargo test            # full suite passes
   ```
4. Stop with changes **uncommitted** for review.
5. Commit **only on explicit approval**, then move to the next task.

## Architecture — single crate, sliced by domain

- `core/<slice>/{primitives,service}.rs` — domain types + orchestration, **zero I/O**.
- `core/deps/<slice>/*.rs` — **ports**: `#[async_trait]` traits used behind
  `Arc<dyn Trait>`, **one file per port**, each with its own `thiserror` enum
  named `{TraitName}Error`. **Never `Result<T, String>`.**
- `adapters/<slice>/…` — port implementations (DB, HTTP, signer, RPC) + inbound handlers.
- Composition root wires adapters → services → state. walletkit is a **library**,
  so the composition root is the **facade** (`setup.rs`/facade module), not a
  `main.rs` binary — adapt the pattern accordingly.
- Prefer **fewer, larger, focused files** over many tiny ones.

## Code quality (rust-lang/rust AGENTS.md discipline)

- Comments explain **why, not what** — short and minimal; most lines need none.
  **No comment-per-change.**
- Idiomatic, consistent naming: reads/lookups are `get_…`→ prefer bare noun
  accessors (drop `get_`), writes are plain domain verbs, predicates use
  `is_`/`supports_`. Fix outliers to match house style.
- Decompose large functions; reduce LOC where it stays clean.
- **YAGNI** — define a type/field/variant/config knob only when a consumer
  actually needs it. Don't pre-build. Add deps at their first real consumer.
- **Reuse before hand-rolling — do not reinvent the wheel.** If a single library
  function, a built-in/default, or a popular well-maintained crate does the job, use it
  instead of writing the logic yourself; delete code the library already provides. Prefer
  alloy / serde / sqlx / redb / std primitives over bespoke code. Before writing a
  non-trivial routine, ask "does a primitive already do this?" — e.g. redb commits are
  `Durability::Immediate` by *default* (don't set it explicitly), SQL upserts use
  `INSERT … ON CONFLICT` (don't read-modify-write), hex via `alloy`'s `LowerHex` (don't
  format by hand). Weigh a new dependency against its maintenance/stability (an
  unstable-API crate can be worse than 100 clear lines). Keep LOC low and the code
  straightforward.
- **Enforce reuse at plan-writing time.** Every plan step that involves non-trivial logic
  must name the library primitive it will use (or explicitly justify why none fits). Don't
  defer the "is there a primitive for this?" question to implementation — decide it in the
  plan.
- **No `unwrap()`/`expect()` in production code** — they panic. Propagate with `?`
  through the per-port `{TraitName}Error`, or handle explicitly (`unwrap_or`,
  `unwrap_or_default`, `if let`, `let … else`, `match`). Allowed **only** in
  `#[cfg(test)]`, `const` contexts, or a **documented genuinely-infallible invariant**
  where a fallback would be *wrong* (e.g. `serde_json::to_vec` of a plain struct) — the
  `expect` message must state the invariant. Prefer `parking_lot` locks (no poisoning,
  no `.lock().unwrap()`).
- **Prefer `match` and combinators over nested `if/else`.** Model mutually-exclusive
  cases with `match`; flatten with `if let` / `let … else` / `?` / iterator
  combinators (`map`, `filter`, `ok_or`, `and_then`). Deep `if/else` ladders and
  arrow-shaped nesting are a refactor smell — rewrite them.
- **DRY** — no copy-pasted logic. Extract a shared function/type at the second use
  (build+sign+encode, CAS loops, error mapping, etc.); keep the API surface small.

## Observability & error handling (standards set in the errors+observability phase)

New code MUST follow these; they are not optional.

- **One public error type.** Every fallible public API returns `WalletKitError`; per-port
  `{Trait}Error`s stay the internal contracts and map in via `From`. A new failure mode
  must (a) surface as a `WalletKitError` variant and (b) be classified in `kind()`
  (`Retryable` / `Terminal` / `NeedsReconcile`); add a `remediation()` hint when it helps.
  Never return `Result<_, String>` or leak a raw port error across the public boundary.
- **Instrument via the shim, never `tracing::` directly.** Import once per module
  (`use crate::obs::{info, warn, error, debug};`) and call bare (`warn!(...)`); use
  `#[cfg_attr(feature = "tracing", tracing::instrument(...))]` for spans. Never install a
  subscriber, call `set_global_default`, or depend on `opentelemetry` in the crate — the
  host owns that. Builds must stay green **with and without** `--no-default-features`.
- **Correlate by intent hash.** Open or inherit a per-intent span carrying `intent_hash`;
  set a fact on the span once, not on every child event.
- **Levels:** ERROR = terminal caller-facing failure · WARN = recoverable/degraded (bump,
  reorg, replaced, failover, retry) · INFO = sparse milestones (~1/tx: submitted, confirmed)
  · DEBUG = mechanics · TRACE = FSM/raw. Keep INFO sparse — no per-poll INFO.
- **Redaction is mandatory on key paths.** Any fn touching a key, `PolicyApproval`, a tx to
  sign, or signed bytes is `#[instrument(skip_all, fields(<safe allow-list>))]` — allow-list,
  never deny-list. No secret ever becomes a span/event field; secret-bearing types get a
  redacting `Debug`. Keep the redaction test in `signing.rs` green.

## Tests — every test earns its place

- **No tests** for config parsing, serde derives, struct init, route
  registration, or trivial glue.
- **Test only logic that can regress**: orchestration, error/edge paths, SQL
  correctness, non-trivial computation.
- In-memory fakes for core unit tests; a live dependency (gated by env var) for
  adapter integration tests.

## After each task — enforce this checklist

- [ ] Slice layout respected; ports one-per-file with `{TraitName}Error`.
- [ ] YAGNI: no unused types/fields/deps introduced.
- [ ] Reuse check: no logic hand-rolled that a library fn / default / popular crate
      already provides; LOC kept low.
- [ ] Comments are why-not-what; naming matches house style.
- [ ] Only regression-worthy tests added.
- [ ] Public failures return `WalletKitError` (classified via `kind()`); new
      orchestration/adapter code instrumented via `crate::obs`/`instrument`, `skip_all` on
      key paths, no secrets in telemetry; green with and without `--no-default-features`.
- [ ] `cargo fmt --check` + `cargo clippy --all-targets` + `cargo test` run, real
      output reported.
- [ ] Left uncommitted; await review; commit only on approval.
