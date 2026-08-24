# walletkit plans

Design specs and implementation plans, one subdirectory per phase/sub-project. Each holds
a `*-design.md` (the approved spec) and a `*-plan.md` (the task-by-task implementation
plan); the Phase-1 core folder also keeps its refactor notes and test matrices.

Build order is the directory prefix: **0 → a → b → c → d**.

| Dir | Sub-project | Status |
| --- | --- | --- |
| [`0-phase-1-execution-core/`](0-phase-1-execution-core/) | Phase-1 EVM execution core — intent→sign→submit pipeline, tracking executor/FSM, nonce manager, gas oracle, policy engine, transport, facade | merged |
| [`a-observability-errors/`](a-observability-errors/) | One public `WalletKitError` taxonomy + `tracing` instrumentation with redaction | merged |
| [`b-durable-state-recovery/`](b-durable-state-recovery/) | Durable `StateStore` (redb + Postgres) + `FenceToken` seam + crash recovery | merged |
| [`c-signing-surface/`](c-signing-surface/) | Safe EIP-191 / EIP-712 signing — policy-gated, blind-sign-proof, low-s | merged |
| [`d-lifecycle-completeness/`](d-lifecycle-completeness/) | cancel · `Dropped` settling · opt-in intent-refill | planned |

Remaining Phase-1 robustness sub-projects (not yet specced): **E** read-path resilience /
R2, **F** DX seams, **G** supply-chain / CI hardening. Later: SPEC Phase 2 (private
submission + sponsored/meta-tx) onward.
