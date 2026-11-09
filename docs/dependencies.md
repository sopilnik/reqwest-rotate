# Dependencies

Versions are the ones `Cargo.lock` resolves; the requirements are in `Cargo.toml`. Transitive crates are not listed,
except the build scripts and procedural macros that a dependency brings in, which run on the machine that builds the
crate. The crate has no build script of its own and no build dependencies.

The "Build-time code" column covers the whole resolved tree. It was measured with `cargo metadata` and
`cargo tree -e normal,build` (with `dev` added for the development table) for x86_64 Linux, with the default features,
with each optional feature alone and the defaults off, and with `--all-features`. In the runtime table a crate is named
in one row only, so a row that reads "none" can still depend on a crate named elsewhere: h2 reaches `libc` through
tokio. The development table names what a development dependency adds to a default build, so a crate that a runtime
row names for an optional feature, such as `serde_json`, can appear again in the wiremock row. Every proc macro also
brings the build scripts of `proc-macro2` and `quote`. Other targets (`cargo tree --target all`) add
`getrandom`, `rustix`, `rustversion`, `serde_derive`, `jni`, `jni-macros`, `jni-sys-macros`, `wasm-bindgen`,
`wasm-bindgen-macro` and `wasm-bindgen-shared`.

All of these were chosen before this ledger was kept, so "Alternative that lost" reads "not recorded" instead of a
reconstruction.

## Runtime

| Name | Version | Kind | Used for | Why this one | Alternative that lost | Build-time code | Checked |
|---|---|---|---|---|---|---|---|
| h2 | 0.4.20 | normal | Recognising `REFUSED_STREAM` and a remote `GOAWAY(NO_ERROR)` in an error chain, to tell a request that was never processed from one that may have been (`src/retry.rs`). | It is the HTTP/2 crate under `reqwest` and `hyper`, and a downcast only matches the exact type they put in the chain. | not recorded | none | 2026-11-07 |
| http | 1.5.0 | normal | The `http::Error` bound on `RequestBuilder::header`. | The header and error types `reqwest` 0.13 exposes are this crate's. | not recorded | none | 2026-11-07 |
| httpdate | 1.0.3 | normal | Parsing a `Retry-After` header given as an HTTP date. | A small crate that does one job; the date has three legal spellings. | not recorded | none | 2026-11-07 |
| hyper | 1.12.0 | normal, default features off | Downcasting `hyper::Error` (`is_canceled`, `is_user`, `is_parse`) to classify transport failures. | It is the error type `reqwest` wraps, so the downcast has to name this crate. | not recorded | `httparse` (build script), through hyper's HTTP/1 support | 2026-11-07 |
| reqwest | 0.13.5 | normal, default features off; `http2` and `charset` on | The HTTP client that is wrapped. TLS, SOCKS, `query`, `form`, `json` and `multipart` are switched on through this crate's own features. | The crate's purpose is to wrap it; callers who name `reqwest` types need the same 0.13. | not recorded | Through `url`: the build scripts of `icu_normalizer_data` and `icu_properties_data` and the proc macros `displaydoc`, `yoke-derive`, `zerofrom-derive` and `zerovec-derive`. `rustls` (default feature): the build scripts of `rustls`, `aws-lc-rs` and `aws-lc-sys`, the last of which compiles C. `native-tls`: the build scripts of `native-tls`, `openssl` and `openssl-sys`, the last of which links the system OpenSSL, and the `openssl-macros` proc macro. `json`: the `serde_json` and `zmij` build scripts. `multipart`: the `mime_guess` build script. `query`, `form` and `json` also bring the `serde` build scripts, named in its row. | 2026-11-07 |
| serde | 1.0.229 | optional (`query`, `form`, `json`) | The `Serialize` bound of `RequestBuilder::query`, `form` and `json`. | `reqwest`'s own methods take `serde::Serialize`. | not recorded | A build script, in `serde` and in `serde_core` | 2026-11-07 |
| thiserror | 2.0.21 | normal | `Display`, `Error`, `From` and `source` for the public `Error`. | The derive writes `Display`, `source` and `From` for the error enum instead of hand-written impls, and no thiserror type appears in the public API. | not recorded | A build script, and the `thiserror-impl` proc macro | 2026-11-07 |
| tokio | 1.53.3 | normal, default features off; `time` on | `tokio::time` for everything that waits: `Instant` and `sleep_until` in the rate limiter, and `sleep`, `sleep_until`, `Instant` and `timeout` in `src/client.rs` for the wait before the next attempt (the backoff or `Retry-After`) and the time cap on draining a response body, so tests can pause the clock. | `reqwest` runs on tokio. | not recorded | `libc` (build script) | 2026-11-07 |
| tracing | 0.1.44 | optional (`tracing`) | Debug events for retries and proxy rotation. | The crate only emits events through it and installs no subscriber. | not recorded | The `tracing-attributes` proc macro, through tracing's default `attributes` feature | 2026-11-07 |

## Development

| Name | Version | Kind | Used for | Build-time code | Checked |
|---|---|---|---|---|---|
| futures-util | 0.3.34 | dev, default features off | Stream constructors for request and response bodies in tests. | The `futures-macro` proc macro, enabled in this build by another dev dependency | 2026-11-07 |
| reqwest | 0.13.5 | dev, default features off; `stream` on | `Body::wrap_stream` in tests. | As in the runtime table | 2026-11-07 |
| tokio | 1.53.3 | dev; `macros`, `rt-multi-thread`, `test-util`, `net`, `io-util` on | The test runtime, the paused clock, and the raw TCP and HTTP/2 test servers (`net`, `io-util`). | The `tokio-macros` proc macro (`macros`) | 2026-11-07 |
| tracing-subscriber | 0.3.23 | dev, default features off; `fmt` on | Capturing the crate's `tracing` output in a test. | none | 2026-11-07 |
| wiremock | 0.6.5 | dev | The mock origin and proxy servers in the integration tests. | The `serde`, `serde_core`, `serde_json` and `zmij` build scripts | 2026-11-07 |

## Fuzzing

The `fuzz/` crate (`publish = false`, outside the published package, with a `Cargo.lock` of its own) depends on the library by path and on the crate below. It is built only by `cargo fuzz` on a nightly toolchain, and nothing in the published crate's tree changes.

| Name | Version | Kind | Used for | Why this one | Alternative that lost | Build-time code | Checked |
|---|---|---|---|---|---|---|---|
| libfuzzer-sys | 0.4.13 | fuzz crate | The `fuzz_target!` entry point and the bundled libFuzzer that `cargo fuzz` links; `arbitrary` comes with it. | It is the engine `cargo fuzz` is built on, and the Rust fuzzer that OpenSSF Scorecard's fuzzing check detects. | AFL.rs and honggfuzz-rs: separate runners and toolchain setups with no use here beyond what `cargo fuzz` already gives. | A build script that compiles libFuzzer's C++ with `cc` (`cc` is already in the library's lock file). The license is `(MIT OR Apache-2.0) AND NCSA`, accepted for the fuzz crate only in [decision 0007](decisions/0007-libfuzzer-sys-license-in-the-fuzz-crate.md). | 2026-11-08 |

## Release tooling

Used only by `.github/workflows/release.yml`; none of it is part of the published crate or of its lock file.

| Name | Version | Kind | Used for | Why this one | Alternative that lost | Build-time code | Checked |
|---|---|---|---|---|---|---|---|
| cargo-cyclonedx | 0.5.9 | release tool, installed with `--locked` | The CycloneDX software bill of materials that is attached to each release and attested. | Maintained by the CycloneDX project, Apache-2.0. 0.5.10 (2026-11-06) was under the seven-day cooldown when this was chosen, so 0.5.9 (2026-03-19) is pinned. Its own lock file carries RUSTSEC-2026-0009 (`time`), accepted for this tool only in [decision 0008](decisions/0008-cargo-cyclonedx-advisory-accepted-for-the-release-tool.md). | `cargo-sbom`: MIT, one maintainer, last release June 2025. | It is built from source on the runner, so every build script and procedural macro in its own tree runs there; it handles no secret and the job holds only `contents: read`. | 2026-11-09 |
