# 0002 Import grouping is a convention that no command checks yet

Status: accepted, temporary

## Context

The code groups its imports the way rustfmt's `group_imports = "StdExternalCrate"` does: the standard library, then
external crates, then the crate's own, each in its own block. That option is unstable, so only a nightly rustfmt reads
it, and the repository has no `rustfmt.toml` and no pinned nightly (see 0001). The `cargo fmt --all -- --check` that CI
and `CONTRIBUTING.md` run is stable rustfmt and ignores the grouping.

## Decision

The grouping is kept in review. Running `cargo +nightly fmt --all --check -- --config group_imports=StdExternalCrate`
on a dated nightly finds exactly one place that breaks it, the `use super::*;` line of the test module in
`src/retry.rs`. That line is fixed in the change that starts running the check, once a toolchain file pins the nightly.

## Alternatives

- Run the check in CI on `nightly` as it comes. Rejected: a rustfmt change on nightly would turn unrelated pull requests
  red.
- Stop grouping. Rejected: the grouping keeps diffs of the `use` blocks small.

## Consequences

A misgrouped import gets through unless a reviewer sees it.
