# Security Policy

walletkit handles signing keys and authorizes value-bearing transactions. We take
security seriously and appreciate responsible disclosure.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Report privately via GitHub's [private vulnerability reporting](https://github.com/21r21a33333/walletkit/security/advisories/new)
("Report a vulnerability" under the Security tab), or email the maintainer at
`void.00.diwakar@gmail.com` with:

- a description of the issue and its impact,
- steps to reproduce (a minimal proof-of-concept if possible),
- affected version/commit.

We aim to acknowledge within 72 hours and to agree on a disclosure timeline with you.
Please give us reasonable time to release a fix before any public disclosure.

## Scope — areas of particular concern

- Key material handling and any path that could export, log, or leak a private key,
  seed, or signature.
- The `PolicyApproval` → `Signer` gate ("what policy approved is what gets signed") and
  any way to sign without a valid approval.
- Nonce management under the single-writer invariant (double-spend / gap correctness).
- Reorg/finality handling that could produce a false terminal state.

## Supported versions

walletkit is pre-1.0 (`0.0.x`). Security fixes target the latest `main`.
