# 0002 Import grouping is a convention that no command checks yet

Status: accepted, temporary

## Context

The code groups its imports as `group_imports = "StdExternalCrate"` does: the standard library, then external crates,
then the crate's own, in separate blocks. That rustfmt option is unstable and needs a nightly formatter, and the
repository has no `rustfmt.toml` and no pinned nightly (see 0001). The stable `cargo fmt --all -- --check` that CI and
`CONTRIBUTING.md` run does not look at it.

## Decision

The grouping stays a convention, kept in review, until a toolchain file pins a nightly. A run of
`cargo +nightly fmt --all --check -- --config group_imports=StdExternalCrate` on a dated nightly finds one place that
does not follow it: the `use super::*;` line of the test module in `src/retry.rs`. It is fixed in the change that starts
running the check.

## Alternatives

- Check it in CI on a floating `nightly`. Rejected: a formatter change on nightly would fail unrelated pull requests.
- Drop the convention. Rejected: the code is written that way and it keeps diffs of the `use` blocks small.

## Consequences

A misgrouped import is found in review instead of by a command. The check is added together with the pinned nightly.
