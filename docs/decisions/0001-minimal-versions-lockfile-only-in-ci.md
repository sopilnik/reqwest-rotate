# 0001 The lowest-versions lockfile is built only by the CI job

Status: accepted, temporary

## Context

To check the version floors in `Cargo.toml`, every direct dependency has to be resolved to the lowest version its
requirement allows. Cargo can do that only through nightly flags (`-Z direct-minimal-versions` here). The result
depends on two things: the resolver of the nightly that ran it, and what the registry holds that day, because the flag
lowers direct dependencies only and every crate below them still resolves to the newest release its requirements
allow. With no `rust-toolchain.toml` and no recorded nightly date, the same command can give a different lockfile next
month.

## Decision

Only the `minimal-versions` job in `.github/workflows/ci.yml` resolves that lockfile, on the nightly it installs, and
nothing of it is committed: the `Cargo.lock` in the repository is the ordinary one. `CONTRIBUTING.md` does not ask
anyone to resolve it by hand. A toolchain file that pins a nightly will fix the toolchain part of the resolution on any
machine; the crates below the direct ones will still follow the registry.

## Alternatives

- Tell contributors to resolve it by hand on whatever `nightly` they have. Rejected: two contributors would compare
  different lockfiles and neither could tell whether a red run came from the change.
- Pin a nightly in this change. Rejected: a toolchain file and a date to keep current are a change of their own.

## Consequences

A contributor who wants to reproduce a red `minimal-versions` job installs the nightly and the tools that job installs,
and the lockfile can still differ from CI's if the registry has moved in between. 0003 covers the check that uses this
lockfile.
