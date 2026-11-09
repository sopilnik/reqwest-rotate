# 0008 cargo-cyclonedx is installed with an open advisory in its own lock file

Status: accepted, 2026-11-09

## Context

The release workflow (`.github/workflows/release.yml`) builds a CycloneDX software bill of materials with
`cargo-cyclonedx`, installed at the exact version 0.5.9 with `cargo install --locked`. That installs the dependency
versions in the tool's own `Cargo.lock`, which include `time 0.3.36`. The RustSec database lists RUSTSEC-2026-0009 for
it (denial of service by stack exhaustion, fixed in `time` 0.3.47). The same lock file draws three warnings:
`anyhow 1.0.80` (unsound), `rand 0.8.5` (unsound) and `xml-rs 0.8.19` (yanked). The 0.5.10 release has the same
`time` version in its lock file, and the upstream repository had not moved it when this was checked; the pull request
that bumped it was closed unmerged.

The tool runs on the release runner only. It reads `cargo metadata` of this crate and writes a JSON file; it takes no
input beyond the tagged commit and the crates its `Cargo.lock` pins, and the `build` job that runs it holds no secret
and no identity token. It is not
in this crate's `Cargo.lock`, so `cargo deny` and `cargo audit` in CI and in the gate never see it.

## Decision

The advisory is accepted for this one tool and this one use: `cargo-cyclonedx` 0.5.9 in the release workflow. The
accepted risk is a failed or stalled `build` job before anything is published, since the advisory
affects only RFC 2822 parsing, which neither `cargo-cyclonedx` nor `cyclonedx-bom` uses (both use ISO 8601). Nothing from the tool's tree ends up in the published crate.

The acceptance ends when a `cargo-cyclonedx` release has a lock file free of RUSTSEC-2026-0009, at which point the
workflow moves to that release, once it is at least seven days old, and this decision is superseded. If none has come by
2027-02-09, the choice is reviewed again.

`deny.toml` is not changed: the tool is not in the tree that file checks.

## Alternatives

- Another SBOM generator. `cargo-sbom` is the other Rust one; its last release is from June 2025, so it fails the
  maintenance test of the dependency rules. A generator from outside the Rust ecosystem (syft) would bring a new
  toolchain into the release job, and the GitHub dependency-graph export describes the repository's manifests rather
  than the packaged crate with its features.
- Install without `--locked`. That resolves newer `time` and the other crates, but the versions are not fixed or
  reviewed, can be younger than the seven-day cooldown, and differ between releases. Rejected.
- Release without a bill of materials. Rejected: users get the list of what the crate is built from next to its
  attestation, and a release without it would be missing that.

## Consequences

The ledger row for `cargo-cyclonedx` in `docs/dependencies.md` links this decision. Each release checks whether a newer
`cargo-cyclonedx` has a clean lock file before the tag is pushed.
