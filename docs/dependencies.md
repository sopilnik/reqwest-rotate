# Dependencies

Versions are the ones `Cargo.lock` resolves; the requirements are in `Cargo.toml`. Transitive crates are not listed.

## Runtime

| Name | Version | Kind | Used for | Why this one | Alternative that lost | Checked |
|---|---|---|---|---|---|---|
| h2 | 0.4.20 | normal | Recognising `REFUSED_STREAM` and a remote `GOAWAY(NO_ERROR)` in an error chain, to tell a request that was never processed from one that may have been (`src/retry.rs`). | It is the HTTP/2 crate under `reqwest` and `hyper`, and a downcast only matches the exact type they put in the chain. | not recorded | 2026-11-07 |
| http | 1.5.0 | normal | The `http::Error` bound on `RequestBuilder::header`. | The header and error types `reqwest` 0.13 exposes are this crate's. | not recorded | 2026-11-07 |
| httpdate | 1.0.3 | normal | Parsing a `Retry-After` header given as an HTTP date. | A small crate that does one job; the date has three legal spellings. | not recorded | 2026-11-07 |
| hyper | 1.12.0 | normal, default features off | Downcasting `hyper::Error` (`is_canceled`, `is_user`, `is_parse`) to classify transport failures. | It is the error type `reqwest` wraps, so the downcast has to name this crate. | not recorded | 2026-11-07 |
| reqwest | 0.13.5 | normal, default features off; `http2` and `charset` on | The HTTP client that is wrapped. TLS, SOCKS, `query`, `form`, `json` and `multipart` are switched on through this crate's own features. | The crate's purpose is to wrap it; callers who name `reqwest` types need the same 0.13. | not recorded | 2026-11-07 |
| serde | 1.0.229 | optional (`query`, `form`, `json`) | The `Serialize` bound of `RequestBuilder::query`, `form` and `json`. | `reqwest`'s own methods take `serde::Serialize`. | not recorded | 2026-11-07 |
| thiserror | 2.0.21 | normal | `Display`, `Error`, `From` and `source` for the public `Error`. | A derive macro, so it runs at build time. | not recorded | 2026-11-07 |
| tokio | 1.53.3 | normal, default features off; `time` on | `tokio::time::sleep_until` and `Instant` in the rate limiter, so tests can pause the clock. | `reqwest` runs on tokio. | not recorded | 2026-11-07 |
| tracing | 0.1.44 | optional (`tracing`) | Debug events for retries and proxy rotation. | The crate only emits events through it and installs no subscriber. | not recorded | 2026-11-07 |

## Development

| Name | Version | Kind | Used for | Checked |
|---|---|---|---|---|
| futures-util | 0.3.34 | dev, default features off | Stream constructors for request and response bodies in tests. | 2026-11-07 |
| reqwest | 0.13.5 | dev, default features off; `stream` on | `Body::wrap_stream` in tests. | 2026-11-07 |
| tokio | 1.53.3 | dev; `macros`, `rt-multi-thread`, `test-util`, `net`, `io-util` on | The test runtime, the paused clock and the local sockets of the HTTP/2 test server. | 2026-11-07 |
| tracing-subscriber | 0.3.23 | dev, default features off; `fmt` on | Capturing the crate's `tracing` output in a test. | 2026-11-07 |
| wiremock | 0.6.5 | dev | The mock origin and proxy servers in the integration tests. | 2026-11-07 |
