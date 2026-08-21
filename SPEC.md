# walletkit — Design Specification

**Status:** Locked · 2026-08-21
**Scope:** EVM-only (multi-VM deferred) · Rust · client-side facade over [alloy](https://alloy.rs)
**Provenance:** 2 research rounds (~107 + 113 agents, 48 products deep-dived, 8 gap topics) + a 64-agent multi-lens completeness review. All findings adversarially verified.

---

## 1. What walletkit is

A **standalone Rust wallet-infrastructure library**: one ergonomic facade that lets *any* dapp or backend send EVM transactions safely, with keys that never leave a swappable backend, guardrails that cannot be bypassed, and a transaction lifecycle that survives stuck txs and reorgs.

**It is a client-side facade, not a custody service.** It does not run an MPC network, a relay, a bundler, or a mixer — it *integrates* those as pluggable backends/strategies behind narrow traits. First consumers are `arb-router` and `evm-executor`, but the API is general-purpose.

### Design principles
- **Reuse best-in-class infra; build only the orchestration glue that has no off-the-shelf equivalent.** ~80% wraps proven libraries; ~20% is the differentiated glue (see §3, §5).
- **What policy approved is provably what gets signed.** One immutable, context-bound authorization threads through the whole pipeline.
- **Every layer is object-safe and RPC-hoistable** — any layer (policy, key) can be lifted to a remote service behind the identical trait, with no pipeline rewrite.
- **Secure by default:** no plaintext key export, policy on by default, deny-over-allow, blind-signing denied.

---

## 2. Architecture — 11 layers

| Layer | Responsibility |
|---|---|
| **TxIntent + TxContext** *(core primitive)* | One immutable, hashable request (to/value/data/chain/account/purpose). A normalized decoded `TxContext` is derived once; policy, simulate, and sign all bind to the **same** object. |
| **KeyBackend / Signer** | Async, fallible, **signature-only** (never exports). Wraps alloy signers; env/keystore/HD → KMS/HSM/Ledger → TEE/MPC as pure config swaps. First-class `sign_typed_data`, `sign_message` (EIP-191), `sign_7702_authorization`. Every signing op requires a `PolicyApproval`. |
| **Account** *(+ module seam)* | Addressable identity returning stage artifacts (callData, stub signature). EOA now; 7702-EOA & ERC-4337/7579 SmartAccount later behind an `AccountVariant` enum. |
| **PolicyEngine** *(validator + hook)* | Mandatory, un-bypassable gate. Declarative serde rules, **DENY-over-ALLOW + default-DENY**. Decision = `Allow \| Deny{reason} \| RequireApproval{quorum}`. Two phases: `check_before` (pre-sign) + `check_after` (post-confirm accounting). Mints the `PolicyApproval` the signer requires. Config-bound per account, hot-reloadable, with a recovery off-switch. |
| **Permission / SessionGrant** | Scoped, expiring, revocable session keys (signer + policy set + target/selector/spend scope). In-scope txs auto-approve. |
| **Approval + Signature Coordinator** | Resumable, persistable out-of-band quorum collection keyed by intent hash. propose/confirm/fetch; EIP-1271 + ERC-6492 contract signatures; fail-closed timeouts. |
| **SubmissionStrategy** | `submit(signed, prefs, lease)` + symmetric `cancel`. Senders: PublicMempool \| PrivateMev(Flashbots) \| Relayer(ERC-2771) \| Paymaster(4337/7677) \| Eip7702Delegated. Two-phase sponsorship middleware (stub/finalize). `Fallback` combinator. Rotatable endpoint-auth identity distinct from the tx key. |
| **NonceManager** | Local durably-persisted authoritative counter; RPC only for reconciliation (`max(persisted, chain_pending)`). **Pluggable allocation mode** (gapless-sequential default \| parallel-salt/2D \| forwarder-nonce \| EntryPoint-2D). Ownership/fencing seam for HA (see §7). |
| **GasOracle** | EIP-1559 estimation + ≥10%-both-fields bump ladder (1.2×/1.5×/2×) with a hard ceiling that alerts instead of looping. userOp-gas-aware. |
| **TransactionManager** | Validated **ordered stage pipeline** (illegal ordering = construction-time error) + block-triggered **FSM** owning track/bump/retry/cancel/confirm. Stable `TxHandle` across hash-changing bumps. Reorg-aware depth. Suspend/resume for approval flows. Structured lifecycle events. |
| **Wallet facade** | Composition root assembling all layers by config. Every trait object-safe so any layer is RPC-hoistable. |

### Structural rules folded in from review (Phase-1-blocking)
- **R1 — alloy adapter boundary:** walletkit ports *wrap* alloy's generic `Filler`/`Provider`/`Network<N>` behind object-safe facades in concrete adapter structs, and **never** re-export or extend alloy generics across a port. (Resolves the object-safety-vs-generic-filler contradiction.)
- **R2 — read-path transport layer:** a health-aware multi-endpoint RPC abstraction sits under NonceManager/GasOracle/TxManager tracking — symmetric to the SubmissionStrategy `Fallback` on the send path. A stale/forked RPC must not silently corrupt nonce reconciliation or confirmation.
- **R3 — PolicyApproval is an evaluation-context capability** (see §5.1), not a bare intent-hash.
- **R4 — policy gates *signatures*, not just transactions** (see §5.2).
- **R5 — the test harness is a deliverable, not an afterthought** (anvil-fork + deterministic fault-injecting transport; see §8).

---

## 3. Reuse posture

| Concern | Reuse (industry standard) | We do NOT build |
|---|---|---|
| Signing, tx encoding, EIP-712/RLP, providers, fillers, tx-watching | **alloy** (ethers-rs is officially deprecated in its favor) | crypto/RPC primitives |
| HD / mnemonic | **alloy-signer-local** `MnemonicBuilder` (coins-bip39) | our own BIP-32/39 |
| KMS / HSM / Ledger / Trezor | **alloy-signer-aws / -ledger / -trezor** | hardware/KMS drivers |
| MPC / TEE custody | **Turnkey / Fireblocks / Web3Auth / Lit / Circle** APIs as remote signers | MPC crypto (no audited pure-Rust threshold-ECDSA exists) |
| Policy rules backend | optionally **regorus** (MS Rust OPA/Rego) or **cedar** (AWS) | a new policy DSL |
| Multisig / quorum | **Safe contracts + Safe{Core} SDK / Tx Service** patterns | our own multisig |
| Bundlers | talk to **Rundler** (Alchemy, Rust, OSS) / Silius / Alto via 4337 RPC | a bundler |
| Paymasters | **ERC-7677** clients (Pimlico/Alchemy/Gelato) | a paymaster service |
| Private / MEV | **Flashbots** RPC / mev-share | a relay |
| Simulation | **alloy `eth_call`** → **revm** (foundry) / Tenderly | an EVM |
| Durable state | embedded **redb / SQLite (sqlx)** | a storage engine |

**Genuinely-new code (the product):** the `TxIntent`↔`PolicyApproval` binding, the nonce reconciliation policy, the stable `TxHandle` + reorg-aware FSM, and the facade wiring.

---

## 4. Locked decisions

1. **Policy resolution:** DENY-over-ALLOW + default-DENY (ordered first-match offered as an opt-in variant).
2. **Policy→sign gate:** unforgeable `PolicyApproval` **required** by `Signer::sign` — structural, not convention. Designed so in-TEE/MPC policy enforcement drops in later behind the same shape.
3. **TxIntent** is the core primitive from Phase 1; **TxPreview** available from Phase 1, mandatory-before-sign only under approval flows or when the consumer opts in.
4. **Pipeline:** runtime object-safe `Vec<Stage>` with ordering invariants validated at construction (not a compile-time HList).
5. **Nonce:** pluggable allocation mode, default gapless-sequential over a local authoritative counter.
6. **MPC/TEE:** remote signing backends behind the `Signer` trait; never embed MPC crypto. Attestation is an orthogonal optional trait.
7. **On-chain enforcement:** client-side policy shipped honestly as **bypassable** (defense-in-depth); optional Safe-Guard promotion in Phase 5 for trustless enforcement.
8. **ERC-4337:** versioned `UserOperation` enum (v0.6/0.7/0.8) with an explicit pinned per-chain deployment profile, validated at startup.
9. **Runtime:** tokio + object-safe (`Send + Sync`) traits everywhere.
10. **State:** `StateStore` trait + embedded default (redb/SQLite); persistence backend and nonce-ownership/fencing are two separate seams.

---

## 5. Cross-cutting invariants (the must-haves)

### 5.1 PolicyApproval = evaluation-context capability
The token binds `{ intent_hash, sim_digest (block/state-root/simulated-effect snapshot), policy_version, valid_until, allowed_gas_envelope }` and is **single-use** (consumed atomically at sign). Bump/cancel contract: a bump within `allowed_gas_envelope` reuses the approval; exceeding it or materially drifting the sim forces re-evaluation; cancel/replace are themselves intents needing their own approval. `SignerLease` is scoped to `{intent_hash, expiry, max_signings}` so lease and approval agree across the loop. Suspended/pending intents pin `policy_version` (resume under original semantics by default). — *Closes the TOCTOU seam behind the "approved == signed" guarantee.*

### 5.2 Policy gates signatures, not just transactions
Every signing entry point — tx, EIP-712, EIP-191 `personal_sign`, 7702 auth — flows through a mandatory `PolicyApproval`. `personal_sign` mandatorily applies the `0x19` prefix (a signed message can never be a valid tx preimage). Signing arbitrary unstructured bytes that parse as neither a known 712 domain nor a prefixed message is **default-deny**. — *Prevents fund-draining via infinite Permit2 allowance / malicious 7702 delegate / malicious Seaport order without a "transaction" ever existing.*

### 5.3 EIP-712 domain correctness
`sign_typed_data` validates the `EIP712Domain`: assert `verifyingContract`/`chainId` match the intent's target chain/contract; deny domains whose `chainId` mismatches the connected provider; warn-or-deny on omitted `chainId`/`verifyingContract` (cross-chain replay). The typed-data hash binds into the same intent hash policy approved.

### 5.4 Signature hygiene
EIP-2 low-s canonical enforcement in the signature envelope; the envelope carries which key + which scheme (no `ecrecover` assumption). CSPRNG discipline with **fail-closed-on-unavailable** for mnemonics, stealth keys, and nonce salts. Secrets in anti-swap/anti-core-dump locked memory with mandatory redaction in logs/errors.

### 5.5 Unified error taxonomy (Phase 1, not deferred)
One `#[non_exhaustive] WalletKitError` with retryable/terminal/needs-reconcile classification, `.is_retryable()`/`.retry_after()`, machine-readable `ErrorKind`, remediation hints, and a structured `PolicyRejection` naming the exact rule(s) and offending field.

### 5.6 Observability
`tracing` spans correlated by intent hash across every layer + a metrics/`Observer` seam, beyond raw lifecycle events.

---

## 6. Phase roadmap

Each phase ships standalone value. "Reserved seam" = trait shape present in Phase 1, enforcement staged later.

### Phase 1 — EVM Execution Core (MVP)
**Result:** a dapp reliably sends EVM txs through one facade — keys never leave a swappable backend, guardrails can't be bypassed, lifecycle survives stuck txs and reorgs — with no account abstraction.
**Use case:** a backend funds an operational EOA and sends payouts at scale: HD/keystore backend, spend-limit + destination-allowlist policy, `wallet.send(intent)` → gated, simulated, signed, submitted, auto-bumped, confirmed to depth, tracked by a stable id across bumps.
**Ships:** TxIntent+TxContext · KeyBackend (env/EIP-2335 keystore/BIP-44 HD) with `sign_typed_data`/`sign_message`/`sign_7702_authorization` · Account (EOA, stub sig) · PolicyEngine (allow/deny + SpendLimit, deny-over-allow, PolicyApproval mint) · NonceManager (local counter, reconcile, allocate/reset) · GasOracle · SubmissionStrategy (PublicMempool + Fallback) · TransactionManager (staged pipeline + FSM) · Wallet facade · **all 5 must-haves (§5.1–5.5) + structural rules R1/R2** · reserved seams (see §7).

### Phase 2 — Private Submission + Sponsored/Meta-Tx (gasless without full 4337)
**Result:** route the same signed intent through MEV-protected private channels or have a third party pay gas — MEV protection + gasless UX for plain EOAs, selectable per-tx by config.
**Use case:** users transact without ETH via an ERC-2771 relayer; flip a flag to send a high-value swap privately through Flashbots — same intent, same policy, different route.
**Ships:** PrivateMev(Flashbots) · Relayer(ERC-2771) + ForwarderNonce mode · SubmissionPreferences · two-phase sponsorship seam · PolicyEngine SelectorAllowlist + windowed Velocity + check_after · **ApprovalManager + Permit2 / EIP-2612 / EIP-3009 + ApprovalGuard predicate** · calldata-scanner verdict once simulate matures.

### Phase 3 — Co-Sign, Quorum & Approval Workflows
**Result:** require multiple parties / human approval before signing, signatures collected out-of-band over time.
**Use case:** treasury policy "transfer > $50k needs 2-of-3"; the tx parks, collects approver sigs across machines, submits at threshold, fails closed on timeout. Session keys unlocked.
**Ships:** QuorumRule Consensus dimension · ApprovalCoordinator · SignatureCoordinator (content-addressed, **EIP-1271 + ERC-6492**) · TxManager suspend/resume · Permission/SessionGrant · Authenticator (HMAC/API-key → roles/tags) · AML/screening provider co-located here.
**Gold-standard upgrades (scheduled here — this is where they're most needed):**
- **FSM representation → `statig` (hierarchical states).** Phase 1's flat 7-state `TxStatus` uses enum+match (right for runtime-chain-driven, flat states). Quorum/approval introduces *nested* sub-states (`RequireApproval → collecting → threshold-met`, cancel sub-flows) where a hierarchical state machine (`statig`) earns its keep — revisit the executor's state representation then. (Not before: enum+match is correct while states are flat.)
- **Durable execution — append-only event log + replay (beyond the Phase-1 lightweight WAL).** Phase 1's persist-before-broadcast + Recover phase is a right-sized WAL for *fast* sends. Async quorum makes lifecycles *long-running* (hours/days, many restarts) and Phase 3 adds HA/distributed — the regime where a gold-standard durable-execution model (deterministic executor + append-only history over the CAS `StateStore`; or an optional Temporal-style backend behind the `StateStore`/`CoordinationBackend` seam) is worth it. Embeddable-first: a durable backend is opt-in, never a required server.

### Phase 4 — Remote & High-Assurance Custody (KMS/HSM/Ledger/TEE/MPC)
**Result:** upgrade custody from a local secret to cloud KMS, hardware, attested TEE, or threshold MPC as a pure config change; policies can require a minimum assurance level.
**Use case:** env key → AWS KMS → Turnkey-style TEE → Fireblocks-style MPC co-signer, no pipeline/dapp change; policy denies spends > $250k unless signer is Attested-TEE or MPC.
**Ships:** KMS/Turnkey/Fireblocks/Web3Auth remote signers · async error taxonomy for network signers · optional Attestation trait + client verifier · rotate/refresh · AssuranceLevel policy.

### Phase 5 — Account Abstraction (ERC-7702 + ERC-4337 + Paymaster)
**Result:** execute through smart accounts — batching, standardized paymaster-sponsored gas, EOA→smart-account in place — behind the same Account/Submission traits.
**Use case:** one-click batched approve+swap with sponsored gas — 7702 delegation for existing EOAs, counterfactual 4337 + ERC-7677 paymaster for new users; policy gates the delegate target as high-risk.
**Ships:** Eip7702Delegated submission · versioned UserOp (V06/07/08) · Erc4337 bundler client · ERC-7677 paymaster · Safe + Simple7702 SmartAccount backends + ERC-7579 module registry · EntryPoint-2D nonce · **EIP-5792 `wallet_sendCalls`/`getCapabilities`/`getCallsStatus`** · ForwardRequest/7702 cross-chain replay rules (chainId=0 default-deny) · optional Safe-Guard promotion.

### Phase 6 — Privacy (Stealth Addresses + Shielded Pools)
**Result:** recipient privacy + shielded balances, compliance-capable and custody-free, reusing gasless spend paths, strictly opt-in behind cargo features.
**Use case:** payroll pays contractors to fresh ERC-5564 stealth addresses (unlinkable); recipients scan with a viewing key and spend gaslessly; opt-in Railgun/Privacy-Pools adapter where policy requires a compliance proof before unshield.
**Ships:** StealthAddressScheme (ERC-5564/6538) · viewing/scanning key class · ShieldedPool/PrivacyBackend trait (Railgun + Privacy Pools via FFI/sidecar) · PPOI/ASP compliance proofs · policy gates on privacy ops · feature-gated.

### Phase 7 (optional) — Chain Abstraction / Intent Resolution
**Result:** a unified-balance UX where the user signs a chain-agnostic goal and it's fulfilled across chains.
**Use case:** "I want 100 USDC on Base" — resolver decomposes a route and submits a signed **ERC-7683** cross-chain order to external solver networks.
**Ships:** IntentResolver / ERC-7683 client on top of walletkit. **We do not operate a solver or liquidity.** TxIntent is shaped in Phase 1 so this sits on top without a refactor. (Overlaps `arb-router` — build only if we want it in the wallet layer.)

---

## 7. Phase 1 reserved seams & the distributed-nonce commitment

**Reserved seams (trait present now, enforcement staged):** unified error taxonomy w/ retry classification · observability (tracing+metrics) · passkey/P256 secp256r1 + RIP-7212 awareness · low-s enforcement · locked-memory secret handling · supply-chain CI (cargo-deny/audit, pinned lockfile, MSRV, unsafe/panic policy) · crash-recovery rehydration (persist-before-broadcast write ordering) · health-aware read-path RPC failover (R2) · per-chain reorg/finality table · backpressure/admission control · graceful shutdown/drain · policy versioning + hot-reload resume · deterministic `Clock` + seedable RNG seams · `dry_run` preview method · PolicyEngine `validate()`/`explain()` DX · feature-flag matrix (purely-additive, cargo-hack tested) · `ReadClient` (balances/metadata/allowances) · `AccountManager` (HD discovery, `predict_address`) · `NameResolver`(ENS) seam · typestate `Wallet::builder()`.

**Distributed-nonce commitment (concrete must-do, not "later"):** the `NonceManager` + `StateStore` traits ship in Phase 1 with an **ownership/fencing seam** in the `allocate()` contract. Default impl = **single-writer-per-account** with a loudly-documented invariant. A **fencing-token/lease-based distributed impl is a committed roadmap task** (target: Phase 3, when the CoordinationBackend already introduces shared persistence) so HA replicas can share an account without a contract change. The seam exists from day one; only the distributed implementation is deferred.

---

## 8. Testing & verification strategy (a Phase-1 deliverable)

- **`walletkit-test` companion crate:** mock KeyBackend/Signer, in-memory + anvil transport, recording assertions, deterministic Clock + seeded RNG.
- **Anvil-fork integration harness + deterministic fault-injecting transport:** exercises nonce reconciliation, stuck-tx bump/replace, reorg rollback of post-confirm accounting, RPC failover — the reliability surface that is otherwise unverifiable.
- **TDD throughout:** every layer's logic-that-can-regress is tested; no config/serde/struct-init tests.
- **Property tests** for policy algebra (deny-over-allow invariants) and nonce allocation (no gaps, no dup under the ownership model).

---

## 9. Deferred / out of scope
Multi-VM (Solana/Bitcoin) · full privacy beyond stealth+two adapters · operating any solver/relay/bundler/mixer · embedding MPC crypto · trustless on-chain policy (offered only via optional Safe-Guard promotion).
