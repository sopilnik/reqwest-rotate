# 0003 The minimal-versions check runs only in CI

Status: accepted, temporary

## Context

`cargo minimal-versions check --direct --all-features` builds the crate against the lowest allowed version of each
direct dependency. A floor in `Cargo.toml` that is lower than the code really needs, say a `1.1` for an API added in
`1.3`, fails there and nowhere else. Dependabot is set to `versioning-strategy: lockfile-only`, so it never raises
those floors, and nothing else moves or tests them.

## Decision

The `minimal-versions` job in CI is the only place the check runs. The job installs `nightly` without a date, so it
shares the weakness 0001 describes: a red run can come from the nightly, or from a new release of a crate below the
direct dependencies, instead of from the change. That is accepted for now, because the alternative is no guard on the
floors at all. `CONTRIBUTING.md` leaves the check off the list of commands to run before a pull request. Once a
toolchain file pins a nightly, the job should use the same pin.

## Alternatives

- Remove the job until a nightly is pinned. Rejected: a wrong floor would then reach a release unseen.
- Ask contributors to run it locally. Rejected: it needs `cargo-hack`, `cargo-minimal-versions` and a nightly toolchain
  installed, which is a lot to ask for a check CI already runs.

## Consequences

A wrong floor shows up as a red `minimal-versions` job on the pull request, and a nightly or registry change that has
nothing to do with the pull request can turn the same job red.
