# Learning — Phase 2: private routing, end to end

Phase 1 built the seam; Phase 2 filled it. The interesting ideas are less about "call an HTTP endpoint" and more about the correctness properties that make private submission *safe*.

## 1. The privacy-safety invariant is a persistence problem

The headline feature is "send privately." The hard part is *staying* private across the two moments a wallet re-broadcasts a transaction it already sent: a **fee bump** (the tx is stuck, re-sign higher and resend) and **crash-recovery** (the process restarted; rehydrate in-flight txs and resend). If either of those defaulted to the public mempool, a tx the user paid to keep private would leak — silently, and exactly once, which is the worst kind of bug.

The fix is almost boring, and that's the point: persist the route on the `TxHandle` (`#[serde(default)] submission: SubmissionOpts`), and have both re-broadcast paths read it back (`self.submission.submit(rlp, &handle.submission)`). The invariant — *a private tx never touches the public mempool unless explicitly escalated* — reduces to "the route is durable state, not a transient parameter." Phase 1 deliberately made `SubmissionOpts` serializable with a `Public` default so this became a one-field, backward-compatible change: legacy records with no `submission` field deserialize to `Public`, which is exactly what they were.

The lesson: when a correctness property must survive restarts and retries, it belongs in persisted state, and you design the state type in advance so adding it later is a `#[serde(default)]` line, not a migration.

## 2. "Never falsely report sent" — H's ethic, one layer down

Sub-project H proved the executor never falsely reports `Confirmed`. Phase 2 needed the sibling property at the broadcast layer: a relay that rejects a tx (bad auth key, declined for profitability) must **never** be classified as "sent." If it were, the executor would track a phantom in-flight tx that never left the process — and, worse, never escalate or surface the failure.

This is why `RelayAuth`/`RelayRejected` are distinct error variants, and why `is_already_accepted()` returns `false` for them. The executor's existing logic — "already accepted → keep the nonce and let confirm settle it; else → return the error without recording a broadcast" — then does the right thing for free: a relay rejection propagates as an error, the nonce is released, and no phantom broadcast is recorded. The whole safety property is enforced by *one* classification decision (`is_already_accepted` is false), because the surrounding control flow was already correct. Getting the taxonomy right is cheaper than adding new control flow.

There's a subtlety worth internalizing: a relay's `"already known"` / `"nonce too low"` message *should* fold back into the RPC-error path (via `classify`), because on a bump-rebroadcast that genuinely means "already in the pool → settle it." So classification isn't "relay errors are always terminal" — it's "match the semantics, and only the truly-rejected cases become terminal relay errors." The mapping is a pure function precisely so this nuance is unit-testable without a live relay.

## 3. Signing you don't policy-gate

walletkit's whole thesis is that signing is policy-gated — every tx and message passes the `PolicyEngine` before the key touches it. So it's a genuine design question why the `X-Flashbots-Signature` header is signed by a *raw* `PrivateKeySigner`, bypassing the policy gate entirely.

The answer clarifies what the policy gate is *for*: it authorizes **actions with economic or custodial consequence** — moving funds, granting allowances. The endpoint-auth signature has neither; it's an infrastructure credential proving "this request came from our relay account" (for reputation/rate-limiting), signed over the request body, not over anything the user is authorizing. Routing it through the policy gate would be a category error — and it's why the identity is a *separate, rotatable key from the tx key*: compromising it leaks your relay reputation, not your funds. Recognizing which signatures are authorizations and which are just transport credentials is a real security-design skill; conflating them either over-restricts infra or under-protects custody.

## 4. Escalation is a one-way, persisted state transition

`Escalation::PublicAfter { cycles }` trades privacy for liveness after N failed private bumps. The implementation detail that matters: when it fires, it **rewrites the persisted route to `Public`** rather than just sending publicly this once. Why? Because if it only sent publicly for the current bump, a crash-and-recover immediately afterward would read the still-`Private` route and re-hide the tx — flapping between public and private. Making escalation a durable, one-way transition (persisted via the same `put_handle` the bump already does) means the system converges: once public, stays public, and recovery agrees. And it keys off `broadcasts.len()` — the counter Phase 1 noticed already existed — so no new state was needed to count cycles.

## 5. Feature-union: adding a dep without forking the tree

The Flashbots POST needs `reqwest` with a TLS backend. The trap: reqwest's default features include native-tls (openssl), but the alloy tree already uses rustls — naively adding `reqwest` could pull *both* TLS stacks into the build. The fix leans on how Cargo resolves features: they **union** across all dependents. By declaring `reqwest = { default-features = false }` with no TLS feature of our own, we inherit exactly the features alloy-transport-http already turned on (rustls), so the tree stays single-stack. The general principle: when you add a direct dependency on a crate that's already a transitive dependency, specify the *minimum* and let feature-unioning inherit the rest — adding features is safe (union), but turning on a default you don't want (a second TLS stack) is the mistake.

## 6. Why the HTTP path isn't unit-tested (and that's fine)

The `classify` function — status + body → tx hash or classified error — is pure and exhaustively unit-tested (403 → auth, decline → rejected, already-known → accepted, result → hash, 5xx → transient). The actual `reqwest` POST is *not* unit-tested, because a hermetic test would need a mock HTTP server, and the house rule is "every test earns its place." The valuable, regressable logic (classification, routing, persistence, escalation) is all tested through pure functions and in-memory mocks; the thin I/O wrapper around a well-understood HTTP client is not where bugs hide. Knowing which seam to make pure-and-testable, and which to leave as thin untested glue, is what keeps a test suite fast and meaningful instead of broad and brittle.
