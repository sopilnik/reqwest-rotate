# 0007 libfuzzer-sys is accepted under its NCSA license for the fuzz crate only

Status: accepted

## Context

`fuzz/` is a cargo-fuzz crate: it depends on this library by path and on `libfuzzer-sys`, which carries the libFuzzer
runtime that `cargo fuzz` links into each target. `libfuzzer-sys` is published as `(MIT OR Apache-2.0) AND NCSA`. The
NCSA part comes from the libFuzzer sources it bundles from LLVM. The dependency license list this project keeps
(`deny.toml`, `[licenses] allow`) names MIT, Apache-2.0, BSD-3-Clause, ISC and a few others, and not NCSA.

The `fuzz/` crate has a manifest and a lock file of its own and is left out of the published package (`include` in the
root `Cargo.toml` names the files that ship). The license check in CI reads the root manifest only, so it never meets
`libfuzzer-sys`.

## Decision

`libfuzzer-sys` is accepted under `(MIT OR Apache-2.0) AND NCSA` as a dependency of the `fuzz/` crate, and only there.
NCSA is approved by the Open Source Initiative, is permissive and has no copyleft term; the crate that carries it is
never published or shipped, so nothing a user of the library receives is under it.

NCSA stays out of the allow list of `deny.toml`. Any other use of a dependency under NCSA, in the library or in a tool
that ships with it, needs a decision of its own.

## Alternatives

- Add NCSA to the allow list of `deny.toml`. Rejected: that would accept it for the library's own dependency tree, which
  nothing needs, and the root check does not see `fuzz/` anyway.
- Fuzz with another engine, AFL.rs or honggfuzz-rs. Rejected, though their crates are plainer in license terms
  (Apache-2.0 for `afl`; MIT, Apache-2.0, Unlicense or WTFPL for `honggfuzz`): each needs a runner and toolchain setup
  of its own, while `cargo fuzz` is the usual route for Rust libraries and the NCSA part stays in a crate that does not
  ship.
- Leave fuzzing out. Rejected: `ProxyList::new` parses and redacts strings from outside the crate, and fuzzing found a
  password leak in it.

## Consequences

The ledger row for `libfuzzer-sys` in `docs/dependencies.md` links this record. When the fuzz crate gets its own license
check, which belongs with giving `fuzz/` its own dependency updates and audit, that check allows NCSA for
`libfuzzer-sys` alone and refuses it elsewhere. If the crate ever ships, or a dependency of the library comes under
NCSA, this record no longer covers it.
