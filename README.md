# reqwest-rotate

[![crates.io](https://img.shields.io/crates/v/reqwest-rotate.svg)](https://crates.io/crates/reqwest-rotate)
[![docs.rs](https://docs.rs/reqwest-rotate/badge.svg)](https://docs.rs/reqwest-rotate)
[![CI](https://github.com/sopilnik/reqwest-rotate/actions/workflows/ci.yml/badge.svg)](https://github.com/sopilnik/reqwest-rotate/actions)

A small `reqwest` client wrapper for scrapers and API clients that need proxy
rotation, per-host rate limiting, and retry-with-backoff, without pulling in
a middleware framework: one `RotatingClient`, one builder, boring behavior.

## Installation

```sh
cargo add reqwest-rotate
```

or add it to `Cargo.toml` directly:

```toml
[dependencies]
reqwest-rotate = "0.1"
```

TLS defaults to `rustls`. To use your platform's own TLS library instead,
turn off default features and enable `native-tls`:

```toml
[dependencies]
reqwest-rotate = { version = "0.1", default-features = false, features = ["native-tls"] }
```

## Example

```rust
use reqwest_rotate::RotatingClient;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), reqwest_rotate::Error> {
    let client = RotatingClient::builder()
        .proxies([
            "http://user:pass@proxy1.example:8000",
            "http://user:pass@proxy2.example:8000",
        ])
        .rate_limit(Duration::from_millis(500)) // min interval per host
        .retries(4)
        .backoff(Duration::from_millis(200), Duration::from_secs(30))
        .proxy_cooldown(Duration::from_secs(60))
        .timeout(Duration::from_secs(20))
        .user_agent("my-scraper/0.1")
        .build()?;

    let response = client.get("https://example.com/api").await?;
    println!("status: {}", response.status());
    Ok(())
}
```

Proxies are optional. Call `.build()` without `.proxies(..)` and you get a plain
rate-limited, retrying client with sane timeouts.

## What it does

**Proxy rotation.** Round-robin over the list you configure. A proxy that fails to
connect, times out, drops the connection or answers `407` goes on cooldown and is
skipped until the cooldown expires or the proxy answers again. If another proxy is out
of cooldown the retry goes through it immediately; if none is, retries fall back to
the backoff. Any other status is the origin's answer: you get it back, and the proxy
stays healthy.

Only the proxies you configure are used. `HTTP_PROXY` and friends are ignored.
`http://`, `https://` and bare `host:port` work out of the box. `socks5://` and
friends need the `socks` feature; without it they are rejected when the client is
built, not silently on every request. Credentials never reach `Debug` output, error
messages or `tracing` events, and logged URLs drop their query string, so a token
passed as a query parameter stays out of the log too.

**Per-host rate limiting.** A minimum interval between requests to the same host,
enforced with `tokio::time`. Concurrent callers to one host are serialized, not
dropped.

**Retry with backoff.** `408`, `429` and `503` are retried for every request. Other
`5xx` (except `501`/`505`), request timeouts and connections dropped before a response
arrived are retried for idempotent methods only, so a `POST` is never duplicated. The
one exception is a `407` from a proxy, which never forwarded the request. Connect
failures and HTTP/2 `REFUSED_STREAM` are retried for everything.

Delays are full-jitter exponential with a configurable cap. A `Retry-After` header
(seconds or HTTP-date) replaces the computed delay; ask for longer than
`max_retry_after` (30 s by default) and you get the response instead of an early retry.
Before a retry, roughly 64 KiB of the failed response's body is read so the connection
can be reused. A bigger error page costs a reconnect on HTTP/1 or a reset stream on
HTTP/2, not the memory to buffer it. After the last of N+1 attempts you get the
response as-is: check `status()`, or call `error_for_status()`, exactly as with
`reqwest`.

**Timeouts by default.** 30 s per attempt, 10 s to connect, so a proxy that black-holes
connections cannot hang a request forever. Both are configurable. They bound one
attempt, not the whole call; wrap it in `tokio::time::timeout` for a hard overall
budget.

**Not just GET.** `request()` returns a builder wrapping `reqwest::RequestBuilder`;
calling `.send()` on it routes the request through the same rate limiting, rotation
and retries as `get()`. (Calling `.send()` on a plain `reqwest::RequestBuilder` sends
directly, with none of that — `request()` hands back a wrapper precisely so that
mistake doesn't compile silently into a scraper that leaks its real IP.) `send()` on
the client itself still takes a plain `reqwest::RequestBuilder`, for one built some
other way. `execute()` takes a pre-built `reqwest::Request`. A streaming body that
cannot be cloned is sent once, without retries.

**Yours to tune.** `configure(|builder| ...)` applies any `reqwest::ClientBuilder`
setting (default headers, redirect policy, TLS) to every underlying client. Features
such as `gzip`, `brotli`, `cookies` or `json` are enabled on your own `reqwest`
dependency, as usual.

`RotatingClient` is `Clone` (cheap; clones share pools, cooldowns and the rate
limiter), `Send + Sync`, `#![forbid(unsafe_code)]`, TLS via `rustls` by default —
disable default features and enable `native-tls` instead to use your platform's own
TLS. An optional `tracing` feature, off by default, logs retries and proxy rotation
at debug level.

## Why not `reqwest-proxy-pool`?

`reqwest-proxy-pool` is a proxy pool *middleware*: it manages proxy
selection as a layer you plug into your own client stack. `reqwest-rotate`
is the opposite trade-off: one ready-to-use client that bundles rotation,
rate limiting, and retry together, for when you want a working scraper
client and don't want to assemble the pieces yourself.

## Minimum supported Rust version

MSRV is 1.85 (edition 2024), checked in CI by `cargo check --all-features`
on that toolchain. The library only: the dev-dependencies need a newer
compiler.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
