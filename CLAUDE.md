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
- Reuse before hand-rolling: prefer alloy / serde / library primitives over
  bespoke code.

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
- [ ] Comments are why-not-what; naming matches house style.
- [ ] Only regression-worthy tests added.
- [ ] `cargo fmt --check` + `cargo clippy --all-targets` + `cargo test` run, real
      output reported.
- [ ] Left uncommitted; await review; commit only on approval.
