# Contributing

Small pull requests are easier for me to review, so they get merged faster.

## Before you start

If you want to change behavior or the public API, open an issue first so we can talk about it. If the issue already exists, comment on it before you start so two people don't work on the same thing. For a first pull request, try an issue labeled `good first issue`.

## Making a change

- Add a test for any behavior change or bug fix. Integration tests go in `tests/`, in the file for their topic (shared helpers are in `tests/common/mod.rs`); unit tests go next to the code in `src/`.
- Public items need doc comments. CI fails without them.
- The crate docs on docs.rs are `README.md`; its `rust` example runs as a doctest, so keep it compiling.
- The crate has `#![forbid(unsafe_code)]`, and I plan to keep it.
- If users will notice the change, add a line to `CHANGELOG.md` under `## [Unreleased]` (create it if it's missing), in a group like `### Fixed`.
- Examples go in `examples/` and must build with default features; CI's default-features clippy compiles them.

## Checks

CI runs these on stable Rust. Please run them locally before you open a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo clippy --all-targets --no-default-features --features native-tls --locked -- -D warnings
cargo test --locked
cargo test --all-features --locked
cargo test --no-default-features --features native-tls --locked
cargo check --no-default-features --features rustls-tls --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
cargo deny check
```

Everything except the last line comes with rustup. For that one: `cargo install cargo-deny --locked`.

Because of `--locked`, commit `Cargo.lock` if your change updates it.

CI also runs `cargo check --all-features --locked` on Rust 1.85, the minimum supported version. Only the library is checked there, because the dev-dependencies need a newer compiler. If your change needs a newer Rust version, say so in the pull request.

On top of that, CI runs the test suite on macOS and Windows, checks every feature on its own and in pairs, checks that the crate still builds with the lowest dependency versions `Cargo.toml` allows, and compares the public API with the last release; none of that needs to run locally.

## Pull requests

Say what you changed and why, and link the issue, for example `Fixes #123`. Please send unrelated formatting or refactoring as a separate pull request.

## License

reqwest-rotate is licensed under either of the Apache License, Version 2.0 or the MIT license, at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
