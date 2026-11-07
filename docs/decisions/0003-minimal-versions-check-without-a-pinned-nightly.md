# 0003 The minimal-versions check runs only in CI

Status: accepted, temporary

## Context

`cargo minimal-versions check --direct --all-features` builds the crate against the lowest allowed version of each
direct dependency, which keeps the version floors in `Cargo.toml` honest. It needs the lockfile of 0001 and nightly
Rust, so it has the same reason to wait for a pinned nightly.

## Decision

The `minimal-versions` job in CI, which installs nightly itself, is the one place that runs the check, and
`CONTRIBUTING.md` does not list it among the commands to run before a pull request. A toolchain file that pins a nightly
makes a run on any machine reproducible, and the job can then use the same pin.

## Alternatives

- Document a run on a floating `nightly`. Rejected, as in 0001.
- Remove the CI job as well. Rejected: the floors in `Cargo.toml` are set on purpose and Dependabot does not move them
  (`versioning-strategy: lockfile-only`), so this job is what guards them.

## Consequences

A floor that is too low shows up as a red `minimal-versions` job on the pull request, not before it.
