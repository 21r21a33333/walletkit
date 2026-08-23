<!-- Keep PRs focused. See CONTRIBUTING.md and CLAUDE.md. -->

## What & why

<!-- What does this change and why? Link the SPEC.md phase/section or the plan task. -->

## Changes

-

## Verification

- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --all-targets` — zero warnings
- [ ] `cargo test --all-targets` passes (paste the result line)

```
<!-- paste the test result line(s) here -->
```

## Checklist

- [ ] Hexagonal layering respected; ports one-per-file with `{TraitName}Error`
- [ ] No `unwrap()`/`expect()`/`panic!` in production code
- [ ] YAGNI: no unused types/fields/deps introduced
- [ ] Only regression-worthy tests added; comments are why-not-what
- [ ] No secrets in logs/traces (redaction on any signing path)
