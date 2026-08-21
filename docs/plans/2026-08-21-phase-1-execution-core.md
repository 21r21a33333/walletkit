# Phase 1 — EVM Execution Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A dapp or backend can reliably send EVM transactions through the walletkit facade — keys never leave a swappable backend, an un-bypassable policy gate stands between intent and *every* signing op, and the tx lifecycle survives stuck txs and reorgs — verified against a live chain via an anvil-fork harness. No account abstraction.

**Architecture:** Hexagonal, single crate (mirrors `evm-executor`): `core/wallet` holds immutable domain primitives + logic, `core/deps` holds object-safe ports, `adapters` holds flat concrete implementations. An immutable `TxIntent` threads through a validated stage pipeline (build → policy → simulate → sign → submit → track); the `PolicyEngine` mints an unforgeable `PolicyApproval` the `Signer` requires, so what policy approved is provably what gets signed.

**Tech Stack:** Rust (edition 2024), alloy / alloy-primitives / alloy-signer-local, tokio, async-trait, thiserror, serde, tracing. Test: anvil (alloy node-bindings), mock ports.

## Global Constraints

- **Edition 2024**, MSRV = rustc 1.97 (workspace pins).
- **Object-safe ports:** every `core/deps` trait is `Send + Sync` and object-safe (RPC-hoisting property). Use `#[async_trait]`.
- **R1 — alloy adapter boundary:** walletkit ports WRAP alloy's generic `Filler`/`Provider`/`Network<N>` behind object-safe facades in concrete adapter structs; NEVER re-export or extend alloy generics across a port.
- **Policy is mandatory & structural:** `Signer::sign*` requires a `PolicyApproval`; policy gates ALL signing ops (tx, EIP-712, EIP-191, 7702), not just transactions. Blind unstructured bytes default-deny.
- **DENY-over-ALLOW + default-DENY** policy semantics.
- **Minimal LOC / reuse:** if a library (alloy, serde, alloy-rlp, secrecy, …) already does it, use it — never hand-roll what a crate provides. Prefer type aliases to newtypes unless a newtype prevents a real bug. Fewest lines that are still clear.
- **Define-when-needed (grow, don't pre-commit):** add a type, field, variant, or method only when a consumer in the current task needs it. Don't define full taxonomies/envelopes up front and reshape later. `#[non_exhaustive]` on public enums so growth is non-breaking.
- **Comments (per rust-lang AGENTS.md):** only trivial comments; no explanatory prose essays. Types and code carry meaning; at most one terse line of doc on a public item.
- **Every test earns its place:** no init/serde/struct-field tests; test only logic that can regress.
- **Commits:** clean messages, **no `Co-Authored-By` trailer**; format (`cargo fmt`) + `clippy` clean + tests green before each commit; confirm before every push.
- **EVM-only** this phase; multi-VM deferred.

---

## File structure (map)

**Domain — `src/core/wallet/`**
- `primitives.rs` — `TxIntent` (+ `IntentHash`, `selector()`), `PolicyApproval` (minimal; `TxContext`/`GasEnvelope`/`SimDigest` deferred — envelope grows at Task 17)
- `signing.rs` — `SigningRequest` payload kinds (Tx / TypedData / Message / Auth7702), `SignatureEnvelope`, `SigningScheme`, EIP-712 domain validation, EIP-191 prefixing, low-s canonicalization
- `errors.rs` — `WalletKitError`, `ErrorKind`, `PolicyRejection`
- `policy.rs` — `Decision`, predicate model, deny-over-allow evaluation, default `PolicyEngine` + predicates (SpendLimit / TargetAllowlist / SelectorAllowlist)
- `transaction_manager.rs` — validated stage pipeline + FSM, `TxHandle`
- `facade.rs` — `Wallet` composition root

**Ports — `src/core/deps/`**
- `key_backend.rs` · `account.rs` · `policy_engine.rs` · `nonce_manager.rs` · `gas_oracle.rs` · `submission.rs` · `state_store.rs` · `rpc.rs` (`Rpc` port) · `clock.rs`

**Adapters — `src/adapters/`**
- `transport.rs` (`Transport` — concrete, non-generic, via OZ Robust-Provider; eRPC-first, R1/R2) · `signers.rs` (env/keystore/HD) · `nonce_store.rs` · `gas_oracle.rs` · `public_mempool.rs` · `state_store.rs`
- policy engines (behind the `PolicyEngine` port): native lives in `core/wallet/policy.rs`; `adapters/policy_regorus.rs` (feature `policy-regorus`); `adapters/policy_wasm.rs` + `wit/policy.wit` (feature `policy-wasm`, wasmtime Component Model)

**Test — `src/testkit/` + `tests/`**
- `testkit` (mock ports, deterministic Clock/RNG) · `tests/anvil_fork.rs` (integration harness + fault injection)

---

## Task list (ordered — details expanded per task during review)

Stage A — Foundation types
- **Task 1:** Crate scaffold + intent primitives (`TxIntent`, `IntentHash`, `selector()`)
- **Task 2:** `PolicyApproval` capability — minimal (just the intent binding); envelope (`GasEnvelope`/`SimDigest`/expiry/version) grows at Task 17 (§5.1)
- **Task 3:** ~~standalone error task~~ — folded. `WalletKitError`/`ErrorKind` are introduced by the first producer (Task 8, `Policy` variant + `kind()`); each later producing task (10/11/12/15/16) adds its own variant + `kind()` arm. `#[non_exhaustive]`, `thiserror`, classify-only (backoff lives in the FSM). (§5.5)
- **Task 4:** ~~standalone~~ — folded into Task 6 + Task 11. Phase 1 defines only the tx-signing entry point + `SignatureEnvelope`, gated by `PolicyApproval`. Multi-payload `SigningRequest` (TypedData/Message/Auth7702) + `SigningScheme` grow in Phase 2/5 when a consumer exists. (§5.2)
- **Task 5:** ~~standalone~~ — deferred. EIP-712 domain validation + EIP-191 prefixing land in Phase 2 (first typed-data/message signing). Low-s is guaranteed by alloy/k256 — assert once in Task 11's signer test, no code. (§5.3, §5.4)

Stage B — Port freeze
- **Task 6:** Object-safe ports for all layers (KeyBackend, Account, PolicyEngine, NonceManager, GasOracle, SubmissionStrategy, StateStore, Rpc, Clock) — the trait freeze

Stage C — Policy engine (engine-agnostic — the `PolicyEngine` port is the reuse seam; all engines plug in behind it)
- **Task 7:** `Decision` + `PolicyRejection` — the universal port contract every engine returns
- **Task 8:** Native engine — `Verdict`/`Policy`/deny-over-allow `evaluate` + `SpendLimit`/`TargetAllowlist` + `DefaultPolicyEngine` (wei-exact, zero-dep, frozen default)
- **Task 8b:** Regorus engine adapter — Rego rules + custom `U256`/address/selector builtins (feature `policy-regorus`)
- **Task 8c:** WASM plugin engine (production) — wasmtime + Component Model + a WIT `policy` interface; hardened sandbox (no ambient WASI, epoch/fuel bound, `StoreLimits`), signed + hash-pinned plugin modules, compiled-module cache (feature `policy-wasm`). Polyglot in-process: Go/JS/Rust/Python policy plugins, `U256`-safe. Sequenced after the Phase-1 core loop is green (no earlier consumer).
- **Task 8d** _(Phase 3)_ Casbin engine — RBAC/ABAC who-dimension (`casbin` crate); needs the Phase-3 identity `PolicyContext`.
- **Task 8e** _(Phase 3)_ Cedar engine — structured principal/action/resource authz (`cedar-policy`); structural-only (64-bit), pairs with native/Regorus for wei via the Composite.
- **Task 8f** _(any phase)_ Remote engine — `tonic` gRPC (our `PolicyDecision` contract) + per-product HTTP adapters (`reqwest`, Fystack/MoonPay); host mints the approval; net error → retryable `Err`.
- **Task 8g** _(Phase 3)_ `CompositePolicyEngine` — combine engines (what + who + custom); all-must-allow + deny-over-allow; host mints one approval.
- **Port refinement (Phase 3):** `PolicyEngine::evaluate` input grows from `intent` to `intent + PolicyContext` (initiator/roles/auth) so the who-dimension engines have a subject; `#[non_exhaustive]` context keeps it non-breaking.

Stage D — Supporting adapters
- **Task 9:** ~~standalone~~ — folded. `Clock`/RNG → Task 17 (FSM); in-memory `StateStore` → into Task 10 (its only consumer). Production `StateStore` (redb/SQLite) plugs in behind the port later.
- **Task 10:** `NonceManager` adapter (best-in-class) — authoritative counter with **reserve/release/recycle** (gap-avoiding free-set) + reconcile (first-use + nonce-too-low), persisted `{next, free}` via a **CAS-retry loop** over `StateStore`, keyed by `NonceScope {account, lane}`. Distributed = a different CAS `StateStore` (Phase 3); 2D lanes = Phase 5. Reuses alloy only for the chain read.
- **Task 11:** KeyBackend adapters — env / keystore / HD (alloy-signer-local), signature-gated, low-s
- **Task 12:** `Transport` — one concrete, non-generic transport reusing **OZ Robust-Provider** (+ alloy Tower layers); `(primary, fallbacks)` covers single-endpoint and in-process failover. **eRPC-first**: recommended production = `Transport::single(erpc_url)`, promoted in README (eRPC owns caching/hedging/quorum/overrides). (R1/R2)
- **Task 13:** `GasOracle` adapter — **source-referenced**: `estimate()` = alloy `eip1559_default_estimator` (2×baseFee + 20th-pct priority); `bump()` = geth's exact replacement rule (integer ceil, both fields, strict-greater + 10% `PriceBump`) + alloy 2×baseFee coverage + hard ceiling. u128 integer math (matches geth big.Int). Adds `Rpc::base_fee`.
- **Task 14:** ~~standalone~~ — folded. For an EOA, building the tx is trivial `alloy::TxEip1559` assembly (done in the Task 16 pipeline); `stub_signature` is a 4337/userOp concept (`eth_estimateGas`/`eth_call` need no signature for EOAs) → the `Account` port + stub land in **Phase 5** (SmartAccount). Gas-**limit** estimation (`Rpc::estimate_gas` + state-drift buffer) lands in Task 16, source-referenced (geth `gasestimator.go` returns minimal gas, no end buffer; Safe 64/63 + 2×, viem +10–20%; EIP-150 63/64).

Stage E — Execution + submission
- **Task 15:** `SubmissionStrategy` — thin `PublicMempool` sender (`rpc.send_raw`). The **seam stays** so Phase 2 adds strategies without a pipeline refactor. **Remaining → Phase 2:** `Fallback` combinator, `PrivateMev` (Flashbots), `Relayer` (ERC-2771), `Paymaster` (4337/7677), `SubmissionPreferences`, explicit `cancel`.
- **Task 16:** `TransactionManager` send pipeline (fixed order: est-gas→fees→simulate→policy→allocate→build→sign→persist→submit) + a **stable, persisted, queryable `TxHandle`** with a `TxStatus` lifecycle (modeled on OZ `transactionId` / thirdweb `queueId` / viem replacement reasons). Sourced gas-limit buffer; nonce allocate-after-allow + release-on-fail. Pluggable `Vec<Stage>` → Phase 2.
- **Task 17:** `TransactionManager` **per-account executor** (thirdweb engine-core pattern, reimplemented) — **Recover → Confirm → Send** loop: recover=rebroadcast persisted in-flight; confirm=**nonce progression** + receipt classify (reorg via block_hash; replacement via viem reasons); send=timeout bump (same nonce, `gas_oracle.bump`) + adaptive in-flight cap. **Grows `PolicyApproval` into the §5.1 envelope** (bumps within it reuse the approval). Adds `Clock` + `Rpc::tx_count`/`block_number`. Reuse alloy RPC + the pattern (not engine-core's Redis service). Largest task — sub-committed per phase.

Stage F — Facade + verification
- **Task 18:** `Wallet` facade — composition root + `send(intent)` end-to-end
- **Task 19:** `testkit` — mock ports + deterministic Clock/RNG
- **Task 20:** Anvil-fork integration harness + fault-injecting transport; happy-path + stuck-tx-bump + reorg tests (§8)

---

## Per-task detail

_Expanded and reviewed one task at a time._

### Task 1 — Scaffold + intent primitives

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/core/mod.rs`, `src/core/deps/mod.rs` (placeholder), `src/core/wallet/mod.rs`, `src/core/wallet/primitives.rs`, `src/adapters/mod.rs`
- Test: inline `#[cfg(test)]` in `primitives.rs`

**Reuse:** `alloy_primitives` (`Address`, `B256`, `Bytes`, `U256`, `TxKind`, `Selector`, `keccak256`); `serde` + `serde_json` for the canonical intent hash (no hand-rolled byte framing). `IntentHash` is a type alias, not a newtype.

**Produces (consumed by Tasks 2, 6, 7, 8, 16):**
```rust
pub type IntentHash = B256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxIntent {
    pub chain_id: u64,
    pub account: Address,
    pub to: TxKind,
    pub value: U256,
    pub input: Bytes,
    pub purpose: Option<String>,
}

impl TxIntent {
    /// Stable content hash — the object policy/simulate/sign bind to.
    pub fn hash(&self) -> IntentHash {
        keccak256(serde_json::to_vec(self).expect("TxIntent is serializable"))
    }

    pub fn selector(&self) -> Option<Selector> {
        match self.to {
            TxKind::Call(_) if self.input.len() >= 4 => Some(Selector::from_slice(&self.input[..4])),
            _ => None,
        }
    }
}
```
`TxContext` (a decoded read-model over the intent) is deferred to Phase 2, where real decoding (EIP-712 structs, calldata scanner) makes it earn its keep. Phase 1 predicates read `TxIntent` directly plus `selector()`.
Canonical-hash note: `serde_json` field order is fixed by struct declaration and these alloy types serialize deterministically (hex strings, no maps), so the hash is stable and collision-resistant within a process. Phase 1 never persists intent hashes across alloy versions; if that changes (suspended-intent resume, Phase 3), switch to explicit `alloy_rlp` encoding.

**Tests (earn their place):**
```rust
#[test]
fn intent_hash_binds_every_field() {
    let base = /* fixed intent */;
    let h = base.hash();
    assert_eq!(h, base.clone().hash());              // deterministic
    // mutate each of chain_id/to/value/input/purpose -> hash differs
}

#[test]
fn selector_only_for_calls_with_calldata() {
    // call w/ >=4 bytes -> Some(selector); <4 -> None; value-only -> None; Create -> None
}
```

**Steps:**
- [ ] Scaffold `Cargo.toml` + module tree (empty `deps`/`adapters` placeholders)
- [ ] Write `primitives.rs` (types above)
- [ ] Write the two tests
- [ ] `cargo test && cargo fmt && cargo clippy --all-targets` — green, zero warnings
- [ ] Commit: `Scaffold crate and add intent primitives`

**Cargo deps this task adds:** `alloy-primitives` (serde), `serde` (derive), `serde_json`, `thiserror`, `tokio` (sync/time/rt/macros), `async-trait`, `tracing`.

### Task 2 — `PolicyApproval` capability (minimal)

**Files:** Modify `src/core/wallet/primitives.rs` (+ re-export in `mod.rs`). No new deps.

**Why:** the unforgeable, single-use token that makes the policy→sign gate structural rather than conventional. Only `PolicyEngine` can mint it; `Signer::sign` requires it. Kept minimal — the evaluation-context envelope (`gas_envelope`/`sim_digest`/`valid_until`/`policy_version`) grows into it at Task 17 when the bump/re-eval loop consumes it. Retrofit-safe: the approval is opaque to the `Signer` port (only `authorizes`/`consume` are called), so adding fields later touches no trait contract.

**Produces (consumed by Tasks 6, 8, 11, 16):**
```rust
// Unforgeable: only the policy layer can mint (crate-private); not serializable,
// so it can't be persisted and replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyApproval {
    intent_hash: IntentHash,
}

impl PolicyApproval {
    pub(crate) fn mint(intent_hash: IntentHash) -> Self {
        Self { intent_hash }
    }

    pub fn authorizes(&self, intent_hash: IntentHash) -> bool {
        self.intent_hash == intent_hash
    }

    // by value: single-use, so a leaked approval can't authorize twice
    pub fn consume(self) -> IntentHash {
        self.intent_hash
    }
}
```
`mint` carries `#[allow(dead_code)]` until Task 8 (the default engine) becomes its caller.

**Test (earns its place):**
```rust
#[test]
fn approval_authorizes_only_its_bound_intent() {
    // authorizes(bound_hash) -> true; authorizes(other_hash) -> false
}
```

**Steps:**
- [ ] Add `PolicyApproval` to `primitives.rs`, re-export from `mod.rs`
- [ ] Write the test
- [ ] `cargo test && cargo fmt && cargo clippy --all-targets` — green, zero warnings
- [ ] Commit: `Add PolicyApproval capability`

### Task 6 — Ports (`core/deps`), minimal Phase-1 surface

Each port is object-safe, `Send + Sync`, `#[async_trait]`. Defines ONLY methods a Phase-1 consumer calls; reuses alloy types instead of custom ones; grows later. (Folds the surviving bit of Task 4: the tx-signing entry point.)

- **`Signer`** (`key_backend.rs`) — signature-only, no export; approval required (structural gate).
  `fn address(&self) -> Address` · `async fn sign_transaction(&self, tx: &TxEip1559, approval: PolicyApproval) -> Result<Signature, WalletKitError>`
  Reuse alloy `TxEip1559`, `Signature` (already low-s canonical). *Defers:* `sign_typed_data`/`sign_message`/`sign_7702` + `SignatureEnvelope`/`SigningScheme` → Phase 2/5.

- **`Account`** (`account.rs`) — assemble tx from intent; stub sig for pre-sign simulate.
  `fn build_unsigned(&self, intent: &TxIntent, nonce: u64, fees: Eip1559Estimation) -> TxEip1559` · `fn stub_signature(&self) -> Signature`
  Reuse alloy `TxEip1559`, `Eip1559Estimation`, `Signature`. EOA only.

- **`PolicyEngine`** (`policy_engine.rs`) — pre-sign gate.
  `async fn evaluate(&self, intent: &TxIntent) -> Decision` (`Decision` from Task 7)
  *Defers:* `check_after` (post-confirm accounting) → Phase 2 (velocity).

- **`NonceManager`** (`nonce_manager.rs`) — gapless allocation + reconcile/gap recovery; single-writer ownership.
  `async fn allocate(&self, account: Address) -> Result<u64, WalletKitError>` · `async fn reset(&self, account: Address, next: u64) -> Result<(), WalletKitError>`
  *Defers:* parallel/2D + distributed fencing → noted for Phase 3.

- **`GasOracle`** (`gas_oracle.rs`) — EIP-1559 estimate.
  `async fn estimate(&self, chain_id: u64) -> Result<Eip1559Estimation, WalletKitError>`
  Reuse alloy `Eip1559Estimation`. *Defers:* bump ladder → Task 17.

- **`SubmissionStrategy`** (`submission.rs`) — send a signed tx.
  `async fn submit(&self, signed_rlp: Bytes) -> Result<TxHash, WalletKitError>`
  Reuse alloy `TxHash` (= `B256`). *Defers:* `cancel`, `Fallback`, preferences → Task 15/17.

- **`StateStore`** (`state_store.rs`) — durable state; Phase 1 needs only the nonce counter.
  `async fn load_nonce(&self, account: Address) -> Result<Option<u64>, WalletKitError>` · `async fn store_nonce(&self, account: Address, next: u64) -> Result<(), WalletKitError>`
  *Defers:* idempotency map, tx handles, pending/quorum intents → their consuming tasks/phases.

- **`Rpc`** (`rpc.rs`) — object-safe read-path facade over alloy `Provider` (R1/R2): the RPC ops the nonce/gas/submission adapters need. (Concrete impl is the `Transport` struct — Task 12.)
  `async fn pending_nonce(&self, account: Address) -> Result<u64, WalletKitError>` · `async fn estimate_fees(&self) -> Result<Eip1559Estimation, WalletKitError>` · `async fn send_raw(&self, rlp: Bytes) -> Result<TxHash, WalletKitError>` · `async fn receipt(&self, tx: TxHash) -> Result<Option<TransactionReceipt>, WalletKitError>` (alloy `TransactionReceipt`, reused directly)
  R1: alloy generics confined inside the adapter (Task 12), never across this port. *Defers:* health-aware multi-RPC failover → Task 12 adapter detail; reorg/block tracking → Task 17.

**Also introduced here (minimal):** `WalletKitError` as an empty `#[non_exhaustive]` enum (with `thiserror::Error`) — the port `Result` types reference it, so it must exist. Variants grow per producer; `kind()`/`ErrorKind` arrive at Task 17 (first classifier).

**Not defined here (no Phase-1-core consumer):** `Clock`/RNG seam → introduced at Task 17 (FSM timeouts). `Decision`/`PolicyRejection`/`Policy` → Task 7.

**Test:** none — these are pure trait declarations (no logic to regress). First real tests arrive with the engine (Task 8) and adapters.

**Steps:** write the port files + `deps/mod.rs` re-exports → `cargo build` (compiles, object-safety checked by the compiler) → `fmt`/`clippy` → Commit: `Define core ports`.

### Task 7 — `Decision` + `PolicyRejection` (port contract)

**Files:** Create `src/core/wallet/policy.rs` (+ re-export in `mod.rs`). These two types are the `PolicyEngine` port's return contract — *every* engine (native/Regorus/WASM/remote) produces them. Implementation note: they're a dependency of the Task 6 `PolicyEngine` port, so they land with it.

**Reuse / minimal:** `thiserror` for `PolicyRejection`'s conditional `Display`.

**Produces (returned by every engine; consumed by Task 16):**
```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("policy denied by rule `{rule}`{}: {reason}", .field.as_ref().map(|f| format!(" (field `{f}`)")).unwrap_or_default())]
pub struct PolicyRejection {
    pub rule: String,
    pub field: Option<String>,
    pub reason: String,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Decision {
    Allow(PolicyApproval),
    Deny(PolicyRejection),
    // RequireApproval { quorum } grows in Phase 3
}
```

**Test (earns its place):**
```rust
#[test]
fn policy_rejection_renders_field_only_when_present() {
    // field Some -> "... (field `to`): ..."; None -> no "(field ...)" segment
}
```

**Steps:** write the two types → test → `cargo test && fmt && clippy` green → Commit: `Add Decision and PolicyRejection port contract`.

### Task 8 — Native engine (frozen default)

**Files:** extend `src/core/wallet/policy.rs`.

**Why:** the zero-dependency, wei-exact in-process engine — the deny-over-allow fold plus the two predicates the Phase-1 use case needs ("spend-limit + destination-allowlist"). **Frozen here:** further/complex policy comes from the Regorus engine (8b) or WASM plugins (8c), never new hand-written predicates. Users may still implement their own `Policy` if they want a native rule. `SelectorAllowlist`/`Velocity` → Regorus/Phase 2.

**Reuse / minimal:** `std::collections::HashSet`, alloy `U256`/`Address`. No new deps.

**Produces (consumed by Task 18 wiring):**
```rust
pub enum Verdict {
    Allow,
    Deny(PolicyRejection),
    Abstain, // no opinion (a guard that only denies when tripped)
}

pub trait Policy: Send + Sync {
    fn check(&self, intent: &TxIntent) -> Verdict;
}

// deny-over-allow, default-deny; mints the approval bound to this intent
fn evaluate(policies: &[Box<dyn Policy>], intent: &TxIntent) -> Decision {
    let mut allowed = false;
    for p in policies {
        match p.check(intent) {
            Verdict::Deny(r) => return Decision::Deny(r),
            Verdict::Allow => allowed = true,
            Verdict::Abstain => {}
        }
    }
    if allowed {
        Decision::Allow(PolicyApproval::mint(intent.hash()))
    } else {
        Decision::Deny(PolicyRejection {
            rule: "default-deny".into(),
            field: None,
            reason: "no policy granted permission".into(),
        })
    }
}

pub struct TargetAllowlist { allowed: HashSet<Address> }

impl Policy for TargetAllowlist {
    fn check(&self, i: &TxIntent) -> Verdict {
        match i.to {
            // abstain (not deny) on non-match so allowlists compose under default-deny
            TxKind::Call(a) if self.allowed.contains(&a) => Verdict::Allow,
            _ => Verdict::Abstain,
        }
    }
}

pub struct SpendLimit { max_value: U256 }

impl Policy for SpendLimit {
    fn check(&self, i: &TxIntent) -> Verdict {
        if i.value > self.max_value {
            Verdict::Deny(PolicyRejection {
                rule: "SpendLimit".into(),
                field: Some("value".into()),
                reason: format!("value {} exceeds cap {}", i.value, self.max_value),
            })
        } else {
            Verdict::Abstain
        }
    }
}

pub struct DefaultPolicyEngine { policies: Vec<Box<dyn Policy>> }

#[async_trait]
impl PolicyEngine for DefaultPolicyEngine {
    async fn evaluate(&self, intent: &TxIntent) -> Decision {
        evaluate(&self.policies, intent)
    }
}
```
Plus minimal constructors (`new`) and `DefaultPolicyEngine`'s `PolicyEngine` impl calling `evaluate`. `TargetAllowlist` is the allow-granter, `SpendLimit` the deny-guard. Safety (why `Abstain` not `Deny` on non-match): a guard only denies when tripped and never grants `Allow`, so a config must include an explicit allow-rule to permit anything — a bare guard leaves everything default-denied (Turnkey's model, the secure default).

**Tests (earn their place):**
```rust
#[test]
fn target_allowlist_allows_only_listed_call_targets() {
    // Call(listed) -> Allow; Call(other) -> Abstain; Create -> Abstain
}
#[test]
fn spend_limit_denies_over_cap() {
    // value > cap -> Deny(field="value"); value <= cap -> Abstain
}
#[tokio::test]
async fn engine_composes_allow_guard_and_default_deny() {
    // engine[TargetAllowlist{A}, SpendLimit{100}]:
    //   (to=A, value=50)  -> Allow
    //   (to=A, value=200) -> Deny (SpendLimit)
    //   (to=B, value=50)  -> Deny (default-deny)
}
```

**Steps:** extend `policy.rs` → tests → `cargo test && fmt && clippy` green → Commit: `Add native policy engine (deny-over-allow + SpendLimit + TargetAllowlist)`.

**Port refinement forced here (applies to Task 6 + Task 8):** `PolicyEngine::evaluate` returns **`Result<Decision, WalletKitError>`**, not `Decision`. Rationale: real engines (Regorus, WASM, remote) can *fail operationally* (policy eval error, plugin trap, network) — distinct from a clean `Decision::Deny`. The caller (TxManager) treats `Err` as **fail-closed** (never sign) and may retry if the error is retryable, while `Decision::Deny` is a terminal denial. Native's impl becomes `Ok(evaluate(...))` (it never errors).

### Task 8b — Regorus engine

**Files:** Create `src/adapters/policy_regorus.rs` (feature `policy-regorus`). Adds optional dep `regorus`.

**Why:** the reusable declarative engine — rules authored in Rego (OPA), so the policy *catalog* grows in config, not hand-written Rust. The one thing Rego can't do is 256-bit math (its numbers are f64), so we register custom Rust builtins for `U256`.

**Reuse / minimal:** `regorus` (parse + eval), `serde_json`. We ship the engine wrapper + builtins + input/result mapping — **not** the policies (those are consumer config; we ship one example Rego).

**Shape (implements the `PolicyEngine` port):**
```rust
pub struct RegorusPolicyEngine { /* pre-built regorus engine template + registered builtins */ }

impl RegorusPolicyEngine {
    // Parse policy + load data + register builtins ONCE (fail fast on bad policy).
    pub fn new(rego_src: &str, data: serde_json::Value) -> Result<Self, WalletKitError> { /* ... */ }
}

#[async_trait]
impl PolicyEngine for RegorusPolicyEngine {
    async fn evaluate(&self, intent: &TxIntent) -> Result<Decision, WalletKitError> {
        // clone the pre-parsed engine (no re-parse), set normalized input, eval query,
        // map { allow: bool, deny: [reason] } -> Decision; eval error -> Err (fail-closed).
    }
}
```
Key pieces:
- **`evm.wei_gt(a, b)` builtin** (+ `wei_gte`/`wei_lt` only when a policy needs them) — parses both args as `U256` and compares exactly. *Why a builtin:* Rego numbers are f64; a wei cap above `u64::MAX` would lose precision without this.
- **Input mapping** `intent -> {chain_id, account, to|null, value(hex), selector|null}` — a flat, policy-friendly object (not raw `TxKind` serde). Addresses/selectors are hex strings, so allowlists are plain Rego set membership (`input.to in data.allowlist`) — no builtin needed.
- **Result convention:** the Rego module sets `allow` (bool) and `deny` (set of reasons); non-empty `deny` → `Decision::Deny`, else `allow` → `Decision::Allow(mint(intent.hash()))`, else default-deny.
- **Production:** policy parsed once at construction; per-eval clones the parsed engine (no re-parse) for concurrency; pin the `regorus` version.

**Tests (earn their place):**
```rust
// example policy: allow if input.to in data.allowlist; deny if evm.wei_gt(input.value, data.cap)
#[tokio::test]
async fn regorus_engine_is_wei_exact_across_the_64bit_boundary() {
    // cap = 100 ETH (1e20 wei, > u64::MAX). value = 200 ETH -> Deny; value = 50 ETH -> Allow.
    // This is the whole point: proves the U256 builtin, which f64 Rego could not do.
}
#[tokio::test]
async fn regorus_non_allowlisted_target_is_default_denied() { /* to not in allowlist -> Deny */ }
```
The wei-exactness test is the load-bearing one — it guards the `U256` builtin that justifies this adapter over plain Rego.

**Steps:** add `regorus` (optional dep + feature) → write `policy_regorus.rs` (engine + `evm.wei_gt` builtin + mappings) + one example Rego → tests → `cargo test --features policy-regorus && fmt && clippy` → Commit: `Add Regorus policy engine with U256 builtins`.

### Task 8c — WASM plugin host (production)

**Files:** Create `wit/policy.wit` + `src/adapters/policy_wasm.rs` (feature `policy-wasm`). Adds optional deps `wasmtime` (component-model + async), `sha2` (hash-pin). Sequenced after the Phase-1 core loop is green.

**Why:** run policy plugins authored in Go/JS/Rust/Python **in-process**, no sidecar. wasmtime does the compilation + sandbox; we own the typed interface + the hardening + the trust boundary.

**Security-critical design point:** the plugin returns **allow/deny only — it never mints a `PolicyApproval`.** The host mints the approval (crate-private, bound to `intent.hash()`) *after* the plugin says allow. A plugin therefore cannot forge authorization; the worst a malicious plugin does is wrongly allow/deny *its own* configured policy, never bypass the binding.

**The WIT contract (`wit/policy.wit`):**
```wit
package walletkit:policy@0.1.0;
interface engine {
    record intent {
        chain-id: u64, account: string, to: option<string>,   // 0x-hex; none = create
        value: string,            // 0x-hex U256 wei (plugin does its own 256-bit math)
        selector: option<u32>, input-len: u64,
    }
    record rejection { rule: string, field: option<string>, reason: string }
    variant decision { allow, deny(rejection) }               // NB: no approval here
    evaluate: func(i: intent) -> decision;
}
world policy-plugin { export engine; }
```

**Host shape (implements the `PolicyEngine` port):**
```rust
pub struct WasmPolicyEngine { /* Arc<Engine>, Arc<Component>, Linker (no WASI) */ }

impl WasmPolicyEngine {
    // Verify module hash == pinned, compile once (cache artifact), build a WASI-free linker.
    pub fn new(wasm: &[u8], pinned_sha256: [u8; 32]) -> Result<Self, WalletKitError> { /* ... */ }
}

#[async_trait]
impl PolicyEngine for WasmPolicyEngine {
    async fn evaluate(&self, intent: &TxIntent) -> Result<Decision, WalletKitError> {
        // fresh Store with StoreLimits + epoch deadline; instantiate; call evaluate;
        // map wit::decision::allow -> Decision::Allow(mint(intent.hash())); deny(r) -> Deny(r);
        // trap / timeout / instantiation error -> Err (fail-closed).
    }
}
```

**Hardening (the production point — each is a `Config`/`Store` setting, not our code):**
- **No ambient capability:** the `Linker` has **no WASI** — no fs/net/env/clock/random. The plugin is a pure function of the `intent` record.
- **Bounded execution:** **epoch interruption** (`Config::epoch_interruption`, `Store::set_epoch_deadline` + a ticker) so a runaway plugin traps instead of hanging the signing path.
- **Memory/table caps:** `StoreLimitsBuilder` via `Store::limiter`.
- **Supply chain:** the `.wasm` is **sha256-pinned** (verified in `new`, before compile) — it gates signing, so an unpinned/tampered module is rejected. (Signature verification layers on top later.)
- **Compiled-artifact cache:** `Component::serialize`/`deserialize` keyed by module hash — compile once, not per start.
- **Deterministic + concurrent:** deterministic `Config` (no threads/nondeterminism); `Engine`/`Component` shared via `Arc` (Send+Sync); a **fresh `Store` per eval** so no state leaks between evaluations.

**Reuse / minimal:** wasmtime does compile + sandbox + limits + component model; host bindings via `wasmtime::component::bindgen!`. We write only the WIT, the `intent↔wit` mapping, hash-pin verify, the `Store` hardening config, and the decision mapping.

**Tests (earn their place — the security properties):**
```rust
// tiny fixture guest plugins in tests/fixtures/ (compiled components)
#[tokio::test]
async fn allow_and_deny_map_to_decision_and_host_mints_the_approval() {
    // fixture that allows iff value < cap -> Allow(host-minted, bound to intent.hash) / Deny
}
#[tokio::test]
async fn runaway_plugin_traps_within_epoch_budget() {
    // infinite-loop fixture -> evaluate returns Err (fail-closed) promptly, never hangs
}
#[test]
fn tampered_module_hash_is_rejected_at_load() {
    // wasm bytes whose sha256 != pinned -> new() returns Err (supply-chain guard)
}
```
The runaway-trap and hash-pin tests are load-bearing — they prove the sandbox and the trust boundary, which is the entire reason to go wasmtime-direct.

**Steps:** add deps + `wit/policy.wit` → `policy_wasm.rs` (host + hardening) → author tiny fixture components → tests → `cargo test --features policy-wasm && fmt && clippy` → Commit: `Add hardened wasmtime policy plugin host`.

### Later-phase policy engines (specced now, built at their phase)

**Shared trust boundary:** every engine — Regorus, WASM, Casbin, Cedar, remote — returns *allow/deny only*; the **host always mints the `PolicyApproval`** bound to `intent.hash()`. No engine can forge authorization.

**Phase-3 prerequisite — port carries identity.** The who-dimension engines need a subject a `TxIntent` doesn't have. So in Phase 3 the port grows:
```rust
async fn evaluate(&self, intent: &TxIntent, ctx: &PolicyContext) -> Result<Decision, WalletKitError>;
// PolicyContext { initiator: Option<Principal>, roles: Vec<Role>, auth: ..., #[non_exhaustive] }
```
Phase-1 engines ignore `ctx`. This is why Casbin/Cedar are Phase 3, not preference.

**Task 8d — Casbin engine** (`policy-casbin`). Wraps `casbin::Enforcer` — async `Enforcer::new(model_conf, policy).await`, then `enforce((sub, obj, act)) -> bool` [docs.rs/casbin]. `evaluate` maps `(ctx.initiator/roles → sub, intent.to → obj, action → act)`. **Production nuance: `Enforcer` is *not* thread-safe → hold it behind a `tokio::sync::RwLock`** (or use `CachedEnforcer`) so our `Send + Sync` adapter is sound. Fits RBAC/ABAC/ReBAC role hierarchies ("treasurer may send payouts"). Reuse `casbin`; no wei math (native/Regorus via the Composite). *Test:* role-allowed → Allow, role-absent → Deny.

**Task 8e — Cedar engine** (`policy-cedar`). Wraps `cedar_policy`: parse policies → `PolicySet`; build `Request::new(principal, action, resource, context)` + `Entities`; `Authorizer::is_authorized(&req, &policies, &entities).decision() == Decision::Allow` [docs.rs/cedar-policy] (wrap in `stacker::grow` for deep policy sets). `principal = ctx identity`, `action = Send/Approve`, `resource = account/target`, `context = {chain, …}`. Structural/role authz only — **64-bit `Long`, so numeric wei guards stay native/Regorus** and combine via the Composite. Reuse `cedar-policy`. *Test:* principal permitted → Allow, forbidden → Deny.

**Task 8f — Remote engine** (`policy-remote`). Two adapters behind the port:
- `RemotePolicyEngine` — our own **gRPC** `PolicyDecision` service via `tonic`+`prost` (typed, versioned, mTLS); `evaluate` sends `(intent, ctx)` → `decision`. Timeouts/transport errors → **retryable `Err`** (fail-closed at the caller).
- Product adapters (`reqwest`) speaking Fystack (Go) / MoonPay (Node) JSON APIs — for teams already running those services.
Host mints the approval on a remote `allow`; the remote can't forge one.

**Task 8g — `CompositePolicyEngine`.** `{ engines: Vec<Arc<dyn PolicyEngine>> }`. `evaluate` runs each: any `Err` → `Err` (fail-closed); any `Deny` → `Deny`; only if **all allow** → `Allow(mint(intent.hash()))` (sub-approvals discarded, one minted). This is how what-dimension (native/Regorus/WASM) and who-dimension (Casbin/Cedar) combine — e.g. `[SpendLimit-via-native, roles-via-Cedar]` must both pass.

Each is feature-gated; a consumer compiles only the engines it uses. All plug behind the **unchanged** port (plus the Phase-3 `ctx` extension) — no reshaping of the Phase-1 work.
### Task 10 — `NonceManager` (best-in-class, CAS-based) + in-memory `StateStore`

**Files:** Create `src/adapters/nonce_store.rs`.

**Why:** an authoritative, gap-avoiding nonce manager (reserve/release/recycle + reconcile) whose atomicity lives in the `StateStore` as compare-and-swap — so the **exact same manager works single-process and distributed**, only the store changes. alloy's manager is rejected (not object-safe, known recovery bugs); we reuse alloy only for the chain read.

**Port refinements forced here (update Task 6):**
- `NonceManager` gains `release(account, nonce)` (recycle an abandoned reservation).
- `StateStore` nonce methods become CAS: `load_nonce_state(scope) -> (NonceState, version)` and `cas_nonce_state(scope, expected_version, &NonceState) -> bool`. Version `0` == absent.
- State is keyed by `NonceScope { account, lane }` (lane defaults to `Eoa`) so distributed stores and 2D lanes are drop-in.

**Reuse / minimal:** `std` `BTreeSet`/`Mutex`/`HashMap`, `serde`, `Arc<dyn _>`. No new deps. No process-local allocation mutex — CAS-retry handles concurrency in-process *and* across replicas.

**Produces:**
```rust
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct NonceState { pub next: u64, pub free: BTreeSet<u64> }

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NonceScope { pub account: Address, pub lane: NonceLane }
impl NonceScope { pub fn eoa(account: Address) -> Self { Self { account, lane: NonceLane::Eoa } } }

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NonceLane { #[default] Eoa /* Key(U192) added in Phase 5 for ERC-4337 2D nonce */ }

#[derive(Default)]
pub struct InMemoryStateStore { m: std::sync::Mutex<HashMap<NonceScope, (NonceState, u64)>> }
// impl StateStore: load returns (state,version) (default,0 when absent);
// cas stores iff current version == expected, bumping version. Trivial; no dedicated test.

pub struct LocalNonceManager { store: Arc<dyn StateStore>, transport: Arc<dyn Rpc> }

#[async_trait]
impl NonceManager for LocalNonceManager {
    async fn allocate(&self, account: Address) -> Result<u64, WalletKitError> {
        let scope = NonceScope::eoa(account);
        loop {
            let (mut st, ver) = self.store.load_nonce_state(scope).await?;
            if ver == 0 { st.next = self.transport.pending_nonce(account).await?; } // reconcile on first use
            let nonce = match st.free.iter().next().copied() {
                Some(n) => { st.free.remove(&n); n }        // recycle lowest freed first
                None => { let n = st.next; st.next += 1; n }
            };
            if self.store.cas_nonce_state(scope, ver, &st).await? { return Ok(nonce); }
            // conflict (another writer/replica advanced) -> retry
        }
    }

    async fn release(&self, account: Address, nonce: u64) -> Result<(), WalletKitError> {
        let scope = NonceScope::eoa(account);
        loop {
            let (mut st, ver) = self.store.load_nonce_state(scope).await?;
            if nonce >= st.next { return Ok(()); }          // not a live reservation
            if nonce + 1 == st.next {
                st.next -= 1;                                // top: shrink high-water
                while st.next > 0 && st.free.remove(&(st.next - 1)) { st.next -= 1; } // absorb contiguous
            } else {
                st.free.insert(nonce);                       // middle gap: recycle later
            }
            if self.store.cas_nonce_state(scope, ver, &st).await? { return Ok(()); }
        }
    }

    async fn reset(&self, account: Address, chain_next: u64) -> Result<(), WalletKitError> {
        let scope = NonceScope::eoa(account);
        loop {
            let (mut st, ver) = self.store.load_nonce_state(scope).await?;
            st.next = st.next.max(chain_next);               // nonce-too-low recovery: forward only
            st.free.retain(|&n| n >= chain_next);            // drop freed nonces consumed on-chain
            if self.store.cas_nonce_state(scope, ver, &st).await? { return Ok(()); }
        }
    }
}
```

**Tests (earn their place — the recycle/reconcile/CAS logic):**
```rust
#[tokio::test] async fn allocates_gaplessly_reconciling_from_chain_on_first_use() { /* empty+chain=5 -> 5,6,7 */ }
#[tokio::test] async fn release_top_shrinks_high_water_and_absorbs_contiguous_freed() {
    // alloc 5,6,7; release 6 (free={6}); release 7 (top, absorbs 6) -> next alloc == 6
}
#[tokio::test] async fn release_middle_recycles_lowest_first() { /* alloc 5,6,7; release 6 -> 6, then 8 */ }
#[tokio::test] async fn reset_moves_forward_only_on_nonce_too_low() { /* next=8; reset(10) -> 10 */ }
#[tokio::test] async fn concurrent_allocations_never_duplicate() {
    // 50 concurrent allocate() -> unique & contiguous (guards the CAS-retry loop)
}
```
(Inline `Rpc` double returns a fixed pending nonce; `InMemoryStateStore` is a trivial map wrapper, no dedicated test.)

**Steps:** write `nonce_store.rs` (+ port refinements) → tests → `cargo test && fmt && clippy` green → Commit: `Add CAS-based LocalNonceManager with reserve/release/recycle`.

### Distributed & 2D/parallel nonce seams (specced now, built at their phase)

**Why CAS in Phase 1:** `LocalNonceManager` is a load → compute → CAS-retry loop. In-process it uses the in-memory CAS store; **distributed is literally a different `StateStore`** — no `NonceManager` change. That is why we adopt CAS now instead of a process-local mutex: it removes the Phase-3 reshape.

**Distributed atomic (Phase 3):** implement the same CAS `StateStore` interface against a shared store —
- `RedisStateStore` — CAS via a Lua script (atomic compare-version-and-set) or `WATCH`/`MULTI`.
- `PostgresStateStore` — optimistic version column (`UPDATE … WHERE version = $expected`) or `SELECT … FOR UPDATE`.
- Redlock noted as a coarser lock-per-`NonceScope` alternative for teams that prefer locks over CAS.
Multi-replica sharing one EOA then allocates without double-use, gaplessly. Drop-in — zero `NonceManager` change.

**2D / parallel nonces (Phase 5, ERC-4337):** EOA nonces are strictly sequential, so parallelism comes from either multiple EOAs (already supported — each is its own `NonceScope`) or the EntryPoint **2D nonce** for smart accounts (192-bit key + 64-bit sequence). Phase 5 adds `NonceLane::Key(U192)` and a `allocate_lane(account, lane)` method (non-breaking; `allocate` stays the `Eoa` lane). Each lane is an independent `{next, free}` sequence keyed by `NonceScope` → parallel UserOps that don't head-of-line-block each other. The keying exists from Phase 1, so this is purely additive.

### Task 11 — KeyBackend / Signer adapters (env / keystore / HD)

**Files:** Create `src/adapters/signers.rs`. Absorbs the folded Task 4 (the tx-signing entry point).

**Why:** the concrete `Signer` backends. One adapter over alloy's `PrivateKeySigner` with three constructors — the three "backends" are just three ways to load the same key type. "Key never leaves" is enforced structurally: the port has **no export method**, and the private key stays inside alloy.

**Port refinement forced here (update Task 6):** `Signer::sign_transaction` takes the `IntentHash` so the signer can enforce the gate — the approval must bind to exactly the intent being signed.

**Reuse / minimal:** `alloy-signer-local` (`PrivateKeySigner`, `MnemonicBuilder`, `decrypt_keystore`), `alloy-consensus` (`TxEip1559::signature_hash`), alloy `Signature`; `zeroize` for input secret strings. No hand-rolled crypto/keystore/HD.

**Produces (consumed by Task 16/18):**
```rust
pub struct LocalSigner { inner: PrivateKeySigner } // holds the key; no export

impl LocalSigner {
    pub fn from_private_key(hex: &str) -> Result<Self, WalletKitError> { /* PrivateKeySigner::from_str */ }
    pub fn from_env(var: &str) -> Result<Self, WalletKitError> { /* read env, zeroize, parse */ }
    pub fn from_keystore(path: &Path, password: &str) -> Result<Self, WalletKitError> { /* decrypt_keystore */ }
    pub fn from_mnemonic(phrase: &str, index: u32) -> Result<Self, WalletKitError> {
        // MnemonicBuilder::<English> at BIP-44 m/44'/60'/0'/0/{index}
    }
}

#[async_trait]
impl Signer for LocalSigner {
    fn address(&self) -> Address { self.inner.address() }

    async fn sign_transaction(
        &self, tx: &TxEip1559, intent_hash: IntentHash, approval: PolicyApproval,
    ) -> Result<Signature, WalletKitError> {
        // structural gate: single-use approval must bind to exactly this intent
        if approval.consume() != intent_hash {
            return Err(WalletKitError::Signer {
                message: "approval does not match intent".into(), retryable: false });
        }
        self.inner.sign_hash(&tx.signature_hash()).await
            .map_err(|e| WalletKitError::Signer { message: e.to_string(), retryable: false })
    }
}
```
Introduces the `WalletKitError::Signer { message, retryable }` variant (per-producer error growth) + its `kind()` arm when `kind()` lands (Task 17).

**Tests (earn their place):**
```rust
#[tokio::test]
async fn sign_is_gated_by_a_matching_approval() {
    // approval bound to intent A, signing intent B -> Err (the §5.2 gate); matching -> Ok(sig)
}
#[test]
fn mnemonic_derives_the_standard_ethereum_address() {
    // known test-vector mnemonic + index 0 -> known address (guards the BIP-44 path)
}
#[tokio::test]
async fn produced_signature_is_low_s_canonical() {
    // assert s <= secp256k1n/2 (§5.4 — alloy guarantees it; we pin it against a future signer swap)
}
```
The approval-gate test is the load-bearing one — it proves signing can't happen without a matching, single-use authorization.

**Not yet:** `sign_typed_data`/`sign_message`/`sign_7702` (Phase 2/5); KMS/HSM/Ledger/TEE/MPC remote signers (Phase 4); locked-memory hardening beyond `zeroize` (§5.4 seam).

**Steps:** add alloy signer features + `zeroize` → write `signers.rs` → tests → `cargo test && fmt && clippy` green → Commit: `Add env/keystore/HD key backends with approval-gated signing`.
### Task 12 — `Transport` (single reuse-based transport; eRPC-first)

**Files:** Create `src/adapters/transport.rs`.

**Why:** reliability without reinvention, and one adapter — not two. `Transport` is a **concrete, non-generic** struct wrapping **OZ Robust-Provider** internally; `(primary, fallbacks)` covers everything: a single endpoint is just `fallbacks = []`, and the recommended production setup points `primary` at an **eRPC** endpoint. No generic type parameter → no generic-instantiation overhead, one simple type.

**eRPC is the promoted RPC layer.** The README/docs prominently recommend configuring the RPC layer with **eRPC** (it owns failover, reorg-cache, hedging, dedup, cross-upstream quorum, rate-limits, method overrides — solving R2 better than any in-process logic). `Transport` still adds retry/timeout around whatever endpoint it's given, so eRPC + `Transport` compose cleanly.

**Receipt type:** `Rpc::receipt` returns alloy's `TransactionReceipt` directly (concrete type — consistent with the other alloy types the ports already use; it already carries `block_number`/`block_hash`/`status`/`gas_used`/`transaction_hash`, so no custom type or mapping). Task 17 reads `receipt.block_hash` for reorg detection.

**Reuse / minimal:** **OZ Robust-Provider** (failover/retry/timeout/resilient-subscriptions) + **alloy Tower layers** (`RetryBackoffLayer`, timeout); alloy `Provider`. We write only the thin `Transport` impl + error/receipt mapping. Introduces `WalletKitError::Rpc { message, transient }`.

**Produces:**
```rust
// The one transport. Concrete (no generics); OZ robust provider pinned to a concrete instantiation inside.
pub struct Transport { /* OZ robust provider (concrete) */ }

impl Transport {
    // fallbacks = [] for a single endpoint (e.g. an eRPC URL — the recommended production setup)
    pub fn new(primary: Url, fallbacks: Vec<Url>) -> Result<Self, WalletKitError> { /* wrap OZ */ }
    pub fn single(url: Url) -> Result<Self, WalletKitError> { Self::new(url, Vec::new()) }
}

#[async_trait]
impl Rpc for Transport { /* pending_nonce / estimate_fees / send_raw / receipt */ }

// OUR logic that can regress: transient (network/timeout/5xx) vs terminal (JSON-RPC method error/4xx)
fn classify_rpc_error(e: &TransportError) -> bool { /* -> transient? */ }
```

**Production topologies (documented, eRPC-first):**
- **Recommended:** run **eRPC** → `Transport::single(erpc_url)`. eRPC owns the RPC-management catalog; walletkit stays thin.
- **No extra infra:** `Transport::new(primary, fallbacks)` — OZ failover/retry/timeout in-process.

**Tests (earn their place):** the heavy lifting is reused (OZ/eRPC — their tests), so our testable surface is the mapping we own:
```rust
#[test]
fn classify_rpc_error_separates_transient_from_terminal() {
    // timeout / connection / 5xx -> transient (retryable); JSON-RPC method error / 4xx -> terminal
}
```
`Transport` end-to-end is exercised in the Task 20 anvil harness (real provider); failover is OZ's tested behavior — we don't re-test reused libs.

**Not yet:** WebSocket resilient subscriptions (OZ supports them — Phase 2 streaming); private/MEV broadcast endpoints (Phase 2). Quorum/hedging/caching delegated to eRPC.

**Steps:** add deps (alloy provider, OZ Robust-Provider, `url`) → write `transport.rs` (`Transport` + `classify_rpc_error`, `receipt` returns alloy `TransactionReceipt`) → **add the eRPC recommendation to the README** → unit-test the classifier → `cargo test && fmt && clippy` green → Commit: `Add Transport (OZ Robust-Provider) with eRPC-first docs`.
### Task 13 — `GasOracle` (production-grade, source-referenced)

**Files:** Create `src/adapters/gas_oracle.rs`.

**Why:** gas pricing that matches what production clients actually enforce — every constant and branch traces to geth/reth/alloy or the EIP, not an estimate. `estimate()` reuses alloy's estimator verbatim; `bump()` reproduces geth's replacement rule exactly (integer math, both fields, strict-greater + threshold) and adds alloy's base-fee coverage so the replacement is includable.

**Source references (each line below cites one):**
- **PriceBump = 10%**, both fields, integer threshold, strict-greater: geth `core/txpool/legacypool/legacypool.go` (`DefaultConfig.PriceBump = 10`) + `legacypool/list.go` (`list.Add`); reth txpool default matches.
- **maxFee = 2×baseFee + priority**, priority = 20th-pct over 10 blocks (min 1 wei): alloy `crates/provider/src/utils.rs` (`eip1559_default_estimator`, `EIP1559_BASE_FEE_MULTIPLIER = 2`, `EIP1559_MIN_PRIORITY_FEE = 1`, `PAST_BLOCKS = 10`, `REWARD_PERCENTILE = 20.0`).
- **base fee ≤ +12.5%/block** (so ×2 ≈ 6 blocks of headroom): EIP-1559 `BASE_FEE_MAX_CHANGE_DENOMINATOR = 8`.

**Port refinements:** `bump` added to the `GasOracle` port (async — reads base fee); `Rpc` gains `base_fee()` (latest block `baseFeePerGas`).

**Reuse / minimal:** alloy `Eip1559Estimation` + `Rpc::estimate_fees` (IS alloy's `eip1559_default_estimator`). Introduces `WalletKitError::GasCeilingExceeded`. No f64 — u128 integer math to match geth's big.Int exactly.

**Produces:**
```rust
pub struct RpcGasOracle {
    rpc: Arc<dyn Rpc>,
    ceiling_max_fee: u128,
    price_bump_pct: u128,       // geth DefaultConfig.PriceBump = 10
    base_fee_multiplier: u128,  // alloy EIP1559_BASE_FEE_MULTIPLIER = 2
}

#[async_trait]
impl GasOracle for RpcGasOracle {
    // reuse alloy's default estimator (20th-pct priority / 10 blocks / min 1 wei; maxFee = 2*baseFee + priority)
    async fn estimate(&self) -> Result<Eip1559Estimation, WalletKitError> { self.rpc.estimate_fees().await }

    async fn bump(&self, prev: Eip1559Estimation) -> Result<Eip1559Estimation, WalletKitError> {
        // geth list.go: new value must clear ceil((100+bump)*old/100) AND be strictly > old, on BOTH fields.
        // Integer ceil (geth uses big.Int) — never f64, so it's exact at low-wei prices.
        let n = 100 + self.price_bump_pct;
        let rbf = |old: u128| old.saturating_mul(n).div_ceil(100).max(old + 1);
        let tip = rbf(prev.max_priority_fee_per_gas);
        let rbf_cap = rbf(prev.max_fee_per_gas);
        // base-fee-aware coverage (alloy formula) so the replacement is includable, not stuck again
        let base_fee = self.rpc.base_fee().await?;
        let coverage = base_fee.saturating_mul(self.base_fee_multiplier).saturating_add(tip);
        let max_fee = rbf_cap.max(coverage);
        if max_fee > self.ceiling_max_fee {
            return Err(WalletKitError::GasCeilingExceeded { ceiling: self.ceiling_max_fee, needed: max_fee });
        }
        Ok(Eip1559Estimation { max_fee_per_gas: max_fee, max_priority_fee_per_gas: tip })
    }
}
```
Each bump is ≥10% over the **previous** attempt (compounds across retries — the geth minimum per replacement); the FSM (Task 17) re-bumps per block until the tx mines or `bump` hits the ceiling. `attempt`-indexed escalation is dropped as unneeded — compounding + base-fee coverage is what production does.

**Nonce interplay (Task 17):** a bump re-signs the **same nonce** (that is RBF); the FSM never allocates a new nonce for a bump and only `release`s/advances it when the tx (original or replacement) is finally mined or dropped.

**Tests (earn their place — the exact geth/alloy rules):**
```rust
#[tokio::test] async fn bump_meets_geth_threshold_and_strict_greater_on_both_fields() {
    // prev{cap,tip}: new cap >= ceil(1.1*cap) AND > cap; new tip likewise (base fee low so RBF dominates)
}
#[tokio::test] async fn bump_low_wei_still_strictly_increases() {
    // prev tip = 1 wei -> ceil(1.1*1)=2 and >1  => tip = 2 (guards geth's low-wei strict-greater nuance)
}
#[tokio::test] async fn bump_covers_base_fee_via_2x_multiplier() {
    // base fee high -> max_fee >= 2*base_fee + tip (alloy coverage), still >= RBF cap
}
#[tokio::test] async fn bump_errors_at_ceiling_instead_of_looping() { /* over ceiling -> GasCeilingExceeded */ }
```
(Mock `Rpc` returns a fixed base fee.) `estimate()` = alloy passthrough, no unit test.

**Not yet:** speed tiers (feeHistory percentiles beyond the 20th default) + external oracles (Blocknative) — port is the seam; userOp gas (4337) → Phase 5.

**Steps:** write `gas_oracle.rs` (+ `Rpc::base_fee`) → tests → `cargo test && fmt && clippy` green → Commit: `Add production-grade GasOracle (geth RBF + alloy base-fee coverage)`.

### Task 15 — `SubmissionStrategy`: thin `PublicMempool` sender

**Files:** Create `src/adapters/public_mempool.rs`.

**Why:** keep the submission **seam** in Phase 1 (pipeline/FSM submit through the port, not `rpc.send_raw` directly) so Phase 2 adds private/relayer/paymaster strategies with no pipeline refactor. The Phase-1 impl is a thin passthrough.

**Reuse / minimal:** alloy `eth_sendRawTransaction` via `Rpc::send_raw` (returned tx hash = `keccak256(rlp(signed))`, computed by alloy). No new logic.

**Produces (consumed by Task 16/17):**
```rust
pub struct PublicMempool { rpc: Arc<dyn Rpc> }

#[async_trait]
impl SubmissionStrategy for PublicMempool {
    async fn submit(&self, signed_rlp: Bytes) -> Result<TxHash, WalletKitError> {
        self.rpc.send_raw(signed_rlp).await
    }
}
```

**Remaining features → Phase 2 (noted):** `Fallback` combinator (choose between strategy *types*), `PrivateMev` (Flashbots `eth_sendPrivateTransaction`/bundles), `Relayer` (ERC-2771), `Paymaster` (ERC-4337/7677), `SubmissionPreferences`, explicit `cancel`. (In Phase 1 a "cancel" is just a `submit` of a replacement tx at the same nonce with bumped gas — no separate method needed.)

**Tests:** none — a thin `rpc.send_raw` passthrough (no logic to regress); exercised end-to-end in the Task 20 anvil harness.

**Steps:** write `public_mempool.rs` → `cargo build && fmt && clippy` → Commit: `Add PublicMempool submission strategy`.

### Task 16 — `TransactionManager`: the send pipeline

**Files:** Create `src/core/wallet/transaction_manager.rs`.

**Why:** the one-shot orchestration that turns an intent into a submitted tx + a trackable handle, in the correct order, reusing alloy for all tx mechanics. Tracking/bump/retry/reorg is Task 17.

**Port refinements:** `Rpc` gains `estimate_gas(call)` and `call` (`eth_call`) — alloy passthroughs. Introduces `WalletKitError::SimulationRejected`.

**Reuse / minimal:** alloy `TxEip1559` (build + rlp), `eth_estimateGas`, `eth_call`. The gas-limit buffer is the only tunable.

**Gas-limit (sourced):** `eth_estimateGas` [geth `eth/gasestimator/gasestimator.go`] returns the *minimal* sufficient gas (internal 63/64 optimistic factor, **no end buffer**), so we apply a configurable state-drift buffer (default +25%), consistent with Safe (`64/63` + 2× outer, [Safe gas docs]) and viem (+10–20%). EIP-150 63/64 rule.

**The pipeline (order is fixed in Phase 1):**
```
1. gas_limit = estimate_gas(intent) * (1 + buffer)          // sourced buffer
2. fees      = gas_oracle.estimate()
3. simulate  = rpc.call(intent) -> Ok | SimulationRejected  // read-only, before sign
4. decision  = policy.evaluate(intent)                      // Deny -> Err (no nonce yet); Allow -> approval
5. nonce     = nonce_manager.allocate(account)              // only after Allow; release() on any later error
6. tx        = TxEip1559 { chain_id, nonce, gas_limit, fees, to, value, input }   // alloy
7. signed    = signer.sign_transaction(&tx, intent.hash(), approval)  // gate: approval must match intent
8. persist   = state_store.put_handle(handle) BEFORE broadcast        // idempotency / crash recovery
9. tx_hash   = submission.submit(signed.rlp())
10.return TxHandle { intent_hash, nonce, tx_hash }
```
Nonce discipline (Task 10): allocate only after policy Allow; if steps 7–9 fail, `release(nonce)` so it doesn't gap. Persist-before-broadcast (step 8) so a crash between submit and ack is reconcilable by the FSM.

**`TxHandle`** — modeled on production handles (OZ Defender `transactionId`, thirdweb Engine `queueId`) that stay **stable across gas bumps** and are the **queryable** id (you query by id, not tx_hash):
```rust
pub struct TxHandle {
    pub id: HandleId,             // stable, survives bumps (= hash(intent_hash, nonce))
    pub intent_hash: IntentHash,
    pub nonce: u64,
    pub status: TxStatus,
    pub broadcasts: Vec<TxHash>,  // original + each bump; the mined one tells ours-vs-replaced
}

#[non_exhaustive]
pub enum TxStatus {               // union of OZ / thirdweb / viem lifecycles
    Pending,                                       // built + persisted, not yet broadcast
    Sent,                                          // in mempool
    Mined { block: u64 },
    Confirmed { block: u64 },                      // N-deep (per-chain reorg table, Task 17)
    Failed { reason: String },                     // reverted on-chain
    Replaced { by: TxHash, reason: ReplacementReason }, // viem's 3 reasons
    Dropped,                                       // fell out of mempool
}
pub enum ReplacementReason { Repriced, Cancelled, Replaced } // viem
```
The handle is **persisted** (StateStore, step 8) so it survives crashes and is queryable by `id` — services depend on this (OZ/thirdweb expose status-by-id + webhooks; our §5.6 `EventSink` emits the transitions). Task 16 returns it at `status = Sent`; the full transitions are Task 17.

**Tracking reuse (Task 17):** confirmation waits reuse alloy's `PendingTransactionBuilder` (`with_required_confirmations`/`watch`/`get_receipt` + its heartbeat) — no hand-rolled polling. What alloy's per-hash watcher does NOT do, and the FSM adds: **replacement detection** (viem repriced/cancelled/replaced — the account nonce advances past ours with a mined hash not in `broadcasts`), **reorg un-mine** (via `TxReceipt.block_hash`), and the **bump loop** (OZ: time-based resubmit at +10% / geth `PriceBump`).

**Pluggable stages deferred:** Phase 1 is a fixed, correctly-ordered `send`. The runtime `Vec<Stage>` with construction-time order-invariant validation (for consumer-inserted custom stages) → Phase 2 (no consumer yet).

**Tests (earn their place — orchestration logic, via mock ports):**
```rust
#[tokio::test] async fn happy_path_runs_stages_in_order_and_returns_a_handle() { /* mocks assert order + handle */ }
#[tokio::test] async fn policy_deny_aborts_before_allocating_a_nonce() { /* Deny -> Err; allocate never called */ }
#[tokio::test] async fn simulation_revert_aborts_before_signing() { /* SimulationRejected -> Err; signer never called */ }
#[tokio::test] async fn a_failed_submit_releases_the_nonce() { /* submit Err -> release(nonce) called */ }
```
(All ports mocked — pure orchestration; real chain path is the Task 20 anvil harness.)

**Steps:** write `transaction_manager.rs` (+ `Rpc::estimate_gas`/`call`) → tests → `cargo test && fmt && clippy` green → Commit: `Add TransactionManager send pipeline`.
### Task 17 — `TransactionManager` executor: per-account Recover → Confirm → Send (engine-core pattern)

**Files:** extend `src/core/wallet/transaction_manager.rs`; create `src/core/deps/clock.rs`; extend `primitives.rs` (approval envelope).

**Architecture (from thirdweb engine-core, adapted):** a **per-account executor** — nonce is per-account, so state is serialized there (not per-tx actors that race on the nonce). Each account's executor runs a **three-phase loop** each cycle (block-/timer-triggered via `Clock`). `TxStatus` is a plain enum (dynamic FSM — negligible overhead; no `statig`/typestate — correct while states are flat). We reuse the *pattern*, not the crate (engine-core is a Redis service); RPC primitives reuse alloy. **Gold-standard upgrades scheduled for Phase 3** (where they're most needed): `statig` hierarchical states (nested quorum/approval sub-states) and durable-execution (append-only event log + replay for long-running approval + HA) — see SPEC.md §Phase 3.

**Absorbs deferrals:** `Clock` port (Task 9); §5.1 `PolicyApproval` envelope + `GasEnvelope`/`SimDigest` (Task 2); `GasOracle::bump` consumer (Task 13); `TxStatus` transitions (Task 16).

**Port refinements:** `Clock { now_unix() }`; `Rpc` gains `tx_count(account, Latest)` (mined nonce — the confirmation signal), `block_number()`, and `receipt` already exists. (No per-hash watcher needed as the primary tracker.)

**Sourced / reused:**
- **Confirmation by nonce progression** (engine-core): when `tx_count(account)` passes our tx's nonce, fetch the receipt(s) to classify — cheaper than per-hash polling, and it *is* the replacement detector.
- **Borrowed-tx / persist-before-broadcast** (engine-core) → the Recover phase = crash recovery (a lightweight WAL over `StateStore`).
- **Nonce recycling** (engine-core "optimistic allocation + recycling") — already our Task 10 reserve/release.
- **Adaptive in-flight cap** (engine-core) — per-account backpressure.
- **Bump** = `gas_oracle.bump` (Task 13, geth+alloy sourced), **same nonce**, on a per-speed timeout (OZ). Confirm depth per-chain reorg table (OZ 12).
- **Replacement reasons** = viem (repriced/cancelled/replaced). Reorg un-mine via `receipt.block_hash`.

**§5.1 approval envelope grows here (from Task 2):**
```rust
pub struct PolicyApproval {
    intent_hash: IntentHash,
    gas_envelope: GasEnvelope,   // fee range policy approved
    sim_digest: SimDigest,
    valid_until: u64,            // Clock-based
    policy_version: u64,
}
```
`Signer` signs iff `intent_hash` matches AND fees ⊆ `gas_envelope` AND `now ≤ valid_until` → a **bump within the envelope reuses the approval** (no re-policy); beyond it / expired / sim-drift / hot-reload → re-evaluate. `GasEnvelope`/`SimDigest` defined here.

**The three-phase executor (per account, per cycle):**
```
RECOVER:  for each persisted non-terminal handle -> re-broadcast its latest signed tx (idempotent)
CONFIRM:  mined = rpc.tx_count(account, Latest)
          for each handle with nonce < mined:
             r = rpc.receipt(one of handle.broadcasts)
             mined-hash ∈ broadcasts ? (r.status ? Failed : Mined→Confirmed at depth N)   // reorg via block_hash
                                     : Replaced{reason}                                     // viem: foreign hash at our nonce
             advance/recycle nonce accordingly (Task 10)
SEND:     for each in-flight handle past its bump timeout & still nonce ≥ mined:
             fees' = gas_oracle.bump(prev)  (ceiling -> alert, stop)
             within approval.gas_envelope ? reuse approval : re-evaluate policy
             re-sign SAME nonce -> submit -> append to handle.broadcasts
          admit new sends up to the adaptive in-flight cap, recycled nonces first
```
Every transition persists the handle and emits a §5.6 event. `cancel(id)` submits a 0-value self-send at the same nonce with bumped fees (handled in SEND).

**What we add over engine-core:** pluggable ports (it's Redis-monolithic), the policy gate + §5.1 envelope bump-reuse, embeddable (no Redis — `StateStore` seam), and the multi-engine policy.

**Tests (earn their place — mock `Rpc`/`Clock`/`GasOracle`/`Signer`/`StateStore`, deterministic `Clock`):**
```rust
#[tokio::test] async fn confirm_advances_on_nonce_progression_at_required_depth() {}
#[tokio::test] async fn reorg_unmine_returns_handle_to_sent() {}                       // block_hash changed
#[tokio::test] async fn replacement_detected_when_foreign_hash_mines_at_our_nonce() {} // viem case
#[tokio::test] async fn send_bump_within_envelope_reuses_approval_else_reevaluates() {}
#[tokio::test] async fn bump_stops_and_alerts_at_gas_ceiling() {}
#[tokio::test] async fn recover_rebroadcasts_persisted_inflight_after_restart() {}
#[tokio::test] async fn in_flight_cap_applies_backpressure_to_new_sends() {}
```

**Note (size):** largest task; implement + commit per phase — Recover, Confirm (+reorg/replacement), Send (+bump/envelope), backpressure.

**Steps:** extend `transaction_manager.rs` (+ `Clock`, approval envelope, `Rpc::tx_count`/`block_number`) → per-phase tests → `cargo test && fmt && clippy` green → Commit(s): `Add per-account TransactionManager executor (recover/confirm/send)`.
