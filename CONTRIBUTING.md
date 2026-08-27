# Contributing to walletkit

Thanks for your interest. walletkit is a client-side Rust wallet-infrastructure
library (a facade over [alloy](https://alloy.rs)). This guide covers the workflow and
the quality bar every change must clear.

## Development setup

- Rust: the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml)
  (current stable). The supported floor (MSRV) is the `rust-version` in
  [`Cargo.toml`](Cargo.toml).
- Optional: [Foundry](https://book.getfoundry.sh/) (`anvil`) to run the localnet
  integration tests in [`tests/`](tests). They **skip cleanly** when `anvil` is not on
  `PATH`, so the suite is a no-op without it.
- Activate the repo hooks once per clone: `git config core.hooksPath .githooks`. The
  `pre-commit` gate blocks comments that violate the standard below (bypass a genuine false
  positive with `git commit --no-verify`).

## The gate — run before every push

CI runs, and you should run locally, exactly:

```sh
cargo fmt --all --check       # formatting clean
cargo clippy --all-targets    # zero warnings (CI treats warnings as errors)
cargo test --all-targets      # full suite passes
cargo doc --no-deps --all-features   # docs build clean (missing docs are errors)
```

Report real output; never claim green without it. CI additionally runs an MSRV build, a
`cargo-hack` feature matrix, and a report-only `cargo-deny` supply-chain scan.

## Documentation & changelog (enforced)

- **Every public item is documented.** The crate denies `missing_docs`, so an undocumented
  `pub` item fails the build. Write the doc as you add the item — one or two sentences,
  **why over what**, with an `# Errors`/`# Panics`/`# Examples` section where it earns one.
  Feature-gated items should render correctly under every feature combination.
- **Update the changelog.** Any change under `src/` must add an entry to `CHANGELOG.md`
  under `[Unreleased]` (Added/Changed/Fixed), citing the PR number. CI enforces this; a
  pure refactor or CI-only PR may carry the `skip-changelog` label instead.
- **Doc comments follow the comment standard** the `pre-commit` hook enforces: no
  dev-process breadcrumbs (`Task 3`, `Phase 2`), no code-history narration (`used to be`,
  `refactored`), no roadmap promises (`will be added`). Describe the code as it is.

## Design & conventions

- Read [`SPEC.md`](SPEC.md) for the architecture, phase roadmap, and cross-cutting
  invariants, and [`CLAUDE.md`](CLAUDE.md) for the working agreement. Both are binding.
- **Hexagonal layering:** `core/<slice>/{primitives,service}` (zero I/O) · `core/deps/*`
  ports (`#[async_trait]`, one file per port, each with its own `{TraitName}Error`) ·
  `adapters/*` implementations. The `Wallet` facade is the composition root.
- **No `unwrap()`/`expect()`/`panic!` in production code** — propagate with `?` or handle
  explicitly. Allowed only in `#[cfg(test)]`, `const` contexts, or a documented
  infallible invariant. Prefer `parking_lot` locks.
- Prefer `match`/combinators over nested `if/else`; DRY; YAGNI (add a
  type/field/knob only when a consumer needs it).
- **Every test earns its place** — test logic that can regress (orchestration, error/edge
  paths, non-trivial computation), not config/serde/struct-init/glue.
- Comments explain **why, not what** — short and minimal.

## Pull requests

- Branch from `main`; keep PRs focused. Fill in the PR template.
- Ensure the gate is green.
- One reviewer approval is required (see [`CODEOWNERS`](.github/CODEOWNERS)).

## Security

Do **not** open public issues for vulnerabilities. See [`SECURITY.md`](SECURITY.md).
