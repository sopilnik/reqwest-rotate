# 0004 `unsafe` is allowed only in the environment-proxy test

Status: accepted

## Context

The library promises to ignore `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and their lowercase forms, so a client built
without proxies never routes through whatever the environment names. Testing that means setting those variables in a
process that builds a client; the simplest such process is the test itself. Since edition 2024, `std::env::set_var`
and `std::env::remove_var` are `unsafe`, because another thread reading the environment at the same time is undefined
behaviour.

## Decision

`unsafe` is allowed in exactly one place: the block in `tests/env_proxy.rs` that sets the six proxy variables and
removes `NO_PROXY` and `no_proxy`.

- `src/lib.rs` keeps `#![forbid(unsafe_code)]`, so the library itself can never contain `unsafe`.
- `Cargo.toml` sets `unsafe_code = "deny"` for the whole package, which covers tests, examples and benches.
- The test carries `#[expect(unsafe_code, reason = "...")]`, and the block carries a `// SAFETY:` comment that says why
  nothing else can read the environment while the variables are set.
- The test is alone in its own binary and sets the variables before it starts a server or a socket. A second test in
  that file would break the argument, so the file has one test.
- Any other `unsafe` is rejected.

## Alternatives

- Do not test the behaviour. Rejected: the promise is in the README and a regression would send traffic around the
  configured proxies, which is the failure this crate exists to prevent.
- Set the variables on a child process instead of the test process. Not needed for one test, and it would add a helper
  binary or a re-exec of the test executable just to avoid the single `unsafe` block.
- Lower the lint to `warn` or allow `unsafe` in all tests. Rejected: it would let new `unsafe` in without a decision.

## Consequences

The one `unsafe` block is easy to find and to review. If the block ever goes away, the `#[expect]` is no longer
fulfilled and rustc warns with `unfulfilled_lint_expectations`; CI runs clippy over all targets with `-D warnings`, so
that warning fails the job and the exception cannot outlive its reason.
