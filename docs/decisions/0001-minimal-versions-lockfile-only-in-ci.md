# 0001 The lowest-versions lockfile is built only by the CI job

Status: accepted, temporary

## Context

The minimal-versions check resolves every direct dependency to the lowest version its requirement allows. The resolver
flag for that (`-Z direct-minimal-versions`) exists only on nightly Rust. A run that is meant to give the same answer
tomorrow as today needs a fixed nightly, and the repository has neither a `rust-toolchain.toml` nor a recorded nightly
date. A floating `nightly` can break for reasons that have nothing to do with this crate.

## Decision

The lowest-versions lockfile is generated only by the `minimal-versions` job in `.github/workflows/ci.yml`, on the
nightly that job installs. `CONTRIBUTING.md` does not ask contributors to run it. Once a toolchain file pins a nightly,
the same lockfile can be built on that nightly on any machine.

## Alternatives

- Document a local run on a floating `nightly`. Rejected: the result changes with the day, so a red run says nothing
  about the change under test.
- Pin a nightly in this change. Rejected: it adds a toolchain file and a date to keep current, which is a change of its
  own.

## Consequences

A floor that is too low is found by CI on the pull request, not on the contributor's machine. Decisions 0002 and 0003
end with the same change.
