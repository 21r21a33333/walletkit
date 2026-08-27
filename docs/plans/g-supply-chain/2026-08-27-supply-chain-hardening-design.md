# G — Supply-chain / CI hardening: design

**Sub-project:** G (the last Phase-1 sub-project; closes Phase 1 once the reserved-seam list is enforced). **Date:** 2026-08-27. **Status:** implemented, pending review.

## 1. Goal & scope

Make walletkit releasable and keep it releasable: codify the invariants we have followed by hand (no-panic, no-unsafe), gate dependency health, guarantee reproducible builds, and prepare crates.io publishing. This is the SPEC §7 reserved seam *"supply-chain CI (cargo-deny/audit, pinned lockfile, MSRV, unsafe/panic policy)."*

**In scope:**
- **Panic / unsafe policy** — `#![forbid(unsafe_code)]`; `deny(clippy::unwrap_used/expect_used/panic)` in non-test code.
- **Rustdoc integrity** — fix the 5 pre-existing broken/private intra-doc links; deny them crate-wide.
- **Dependency hygiene** — `cargo-deny` (advisories · licenses · bans · sources), report-only.
- **Reproducibility** — `--locked` throughout CI; MSRV declared truthfully and CI-pinned; `cargo-hack` feature matrix.
- **Release prep** — `docs.rs` metadata, crates.io package identity, and a publishing checklist (§7).

**Explicitly OUT:**
- **R5 fault-injecting transport** → its own sub-project **H**. SPEC lists R5 as a Phase-1-blocking *structural rule* (a real test-harness build), not CI/config work; it does not belong in G.
- **Actual `cargo publish`** → a release action, gated behind this hardening (see §7 checklist). G declares `publish = false` still.

## 2. Findings that shaped the design

Measured against the real repo, not assumptions:

| Area | Finding | Consequence |
|---|---|---|
| Panic policy | **0** production `unwrap`/`expect`/`panic`/`unsafe` (all 226 `unwrap`s are `#[cfg(test)]`); one documented-infallible `expect` in `TxIntent::hash`. | `forbid`/`deny` land at ~zero cost; the one `expect` gets a scoped `#[allow]` per the house rule. |
| MSRV | Declared `rust-version = "1.85"` **never built** — alloy 2.4.1 requires **1.94.1**. | Correct the manifest to the true floor; pin CI to it. |
| Features | `policy-moonpay` and `pricing` were **never compiled in CI**. | `cargo-hack --each-feature` closes the gap. |
| Lockfile | `Cargo.lock` committed but **no `--locked`** anywhere. | Thread `--locked` through every CI cargo call. |
| Advisories | Only `unmaintained` notices (`fxhash`, `paste`), both **transitive**; no vulnerabilities. | Scope `unmaintained` to workspace deps so the report stays actionable. |
| Licenses | Tree is **entirely permissive**; GPL/LGPL crates are dual-licensed with a permissive OR-arm. | Clean publish; allow-list derived from the actual tree. |
| crates.io name | `walletkit` is **taken** (Worldcoin, actively maintained). | Publish as `walletkit-rs`; keep `[lib] name = "walletkit"`. |

## 3. Decisions

- **Two workflows (structure B).** Deterministic gates that *can* block (fmt/clippy/test/docs/MSRV/feature-matrix) stay in `ci.yml`. Externally-triggered, noisy dependency scanning lives in a **separate, report-only `supply-chain.yml`** on PRs + a weekly cron — so a permanently-informational check never masquerades as a build failure, and advisories filed against unchanged code still surface.
- **Report-only for cargo-deny** (per user direction). `continue-on-error` in CI; `deny.toml` itself stays honest. `unmaintained = "workspace"` keeps the report green-when-clean so a real vulnerability is not buried under un-actionable transitive notices.
- **One tool, not two.** `cargo-deny`'s advisory check reads the same RustSec DB as `cargo-audit` and adds license/ban/source auditing — it is a strict superset, so a separate `cargo-audit` job would be redundant (DRY).
- **MSRV = truth, CI-pinned.** Declare the real floor (`1.94.1`) and fail CI if a dep bump raises it, prompting an honest update rather than manifest drift. The floor tracks alloy's rolling MSRV by design.
- **`--each-feature`, not `--feature-powerset`.** Features are purely additive (SPEC), so per-feature compilation is the right guarantee at ~7 builds instead of ~40 (wasmtime makes the powerset expensive for no added signal).
- **`forbid`, not `deny`, for unsafe.** Verified clean even through `sol!`/wasmtime macro expansion, so `forbid` (unoverridable) is correct.

## 4. Tasks (as implemented)

1. **Panic/unsafe lints** — `[lints.rust] unsafe_code = "forbid"` in `Cargo.toml`; crate attributes in `src/lib.rs` for the `cfg(not(test))` panic bans + `docsrs` cfg. Scoped `#[allow(clippy::expect_used)]` on `TxIntent::hash` with the invariant stated.
2. **Rustdoc fixes + gate** — private-module links (`wasm`, `build`, `primitives`, cfg-gated `redb`/`postgres`) → code spans; `pending_handles` → `[`Self::pending_handles`]`; `#![deny(rustdoc::broken_intra_doc_links, rustdoc::private_intra_doc_links)]`. Verified across default / all / no-default features.
3. **`deny.toml` + `supply-chain.yml`** — permissive allow-list from the real tree; `unmaintained = "workspace"`; report-only workflow (PR + weekly cron).
4. **CI hardening** — `ci.yml` gains `msrv` (1.94.1, `--all-features --locked`), `feature-matrix` (`cargo-hack --each-feature --locked`), a `Docs` step (`RUSTDOCFLAGS=-D warnings`, all-features), and `--locked` on every existing cargo call.
5. **Release metadata** — package `walletkit-rs` + `[lib] name = "walletkit"`, `documentation`, expanded `categories`, `[package.metadata.docs.rs]`. GitHub repo topics/description (applied out-of-band).
6. **CHANGELOG** — `[Unreleased]` Added/Changed/Fixed entries.

## 5. Verification (local, real output in the PR)

`cargo fmt --check` · `cargo clippy --all-targets` {default, `--no-default-features`, `--all-features`} · `cargo test --all-targets` · `cargo doc --no-deps` {default, all, no-default} · `cargo deny --all-features check` (advisories · bans · licenses · sources all ok) · `cargo +1.94.1 check --all-features --locked` · `cargo hack check --each-feature --locked` (8/8).

## 6. Non-goals / deferred

- **R5 fault-injecting transport** → sub-project H.
- **`cargo publish`** → release action (checklist below).
- **Per-item `doc(cfg(...))` feature badges** — infrastructure is in place (`docsrs` cfg + metadata); annotating individual gated items is optional polish, deferrable.
- **`--feature-powerset`** — revisit only if a feature-interaction bug ever appears.

## 7. Publishing checklist (crates.io — execute at release, not in G)

Prerequisite: this sub-project (G) merged, CI green.

1. **Name** — confirm `walletkit-rs` still free (`cargo search walletkit-rs`); `[lib] name = "walletkit"` keeps `use walletkit::…` for dependents. *(Done in Cargo.toml.)*
2. **Version** — bump `version = "0.0.0"` → `0.1.0` (pre-1.0: minor = breaking, per this changelog's policy). Move `[Unreleased]` → `[0.1.0] - <date>`.
3. **Enable publishing** — `publish = false` → remove (or `true`). This is the go/no-go line.
4. **README install snippet** — add `cargo add walletkit-rs` (held out of G so no pre-publish copy-paste errors); note the import name is `walletkit`.
5. **Package sanity** — `cargo publish --dry-run --all-features --locked`; check `cargo package --list` for stray files (`/poc`, local artifacts) and add `exclude` if needed. Confirm `docs.rs` build config is honored (all-features).
6. **License/advisory final pass** — the deny license check is report-only in CI; run `cargo deny check licenses` once **as a hard gate** before publishing, since a copyleft slip is a real problem the moment the crate is public. (Tree is currently all-permissive.)
7. **Homepage** — set the GitHub repo `homepage` to `https://docs.rs/walletkit-rs` once the first version is live (held until publish to avoid a dead link).
8. **Publish + tag** — `cargo publish`; `git tag v0.1.0 && git push --tags`. Optionally add a tag-triggered release workflow later.
9. **Post-publish** — verify the docs.rs build succeeded with all features; confirm the crates.io page renders README, categories, and keywords.

**Open decision for release time:** whether to promote the cargo-deny license check from report-only to a hard gate permanently (step 6 does it once by hand regardless).
