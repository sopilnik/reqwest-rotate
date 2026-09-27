# reqwest-rotate

<div style="display:none">

[![crates.io](https://img.shields.io/crates/v/reqwest-rotate.svg)](https://crates.io/crates/reqwest-rotate)
[![docs.rs](https://docs.rs/reqwest-rotate/badge.svg)](https://docs.rs/reqwest-rotate)
[![CI](https://github.com/sopilnik/reqwest-rotate/actions/workflows/ci.yml/badge.svg?event=push)](https://github.com/sopilnik/reqwest-rotate/actions)

</div>

A small `reqwest` client wrapper for scrapers and API clients that need proxy
rotation, per-host rate limiting, and retry-with-backoff, without pulling in
a middleware framework: one `RotatingClient`, one builder, boring behaviour.

## Installation

```sh
cargo add reqwest-rotate
```

or add it to `Cargo.toml` directly:

```toml
[dependencies]
reqwest-rotate = "0.1"
```

TLS defaults to `rustls` (`rustls-tls` still works as the old feature name),
which checks certificates against the operating system's trust store: on Linux,
install `ca-certificates` in a container image. To use your platform's own TLS
library instead, turn off default features and enable `native-tls`:

```toml
[dependencies]
reqwest-rotate = { version = "0.1", default-features = false, features = ["native-tls"] }
```

## Example

```rust,no_run
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

`examples/` has three more: `get`, `proxy_pool` and `retry_metrics`.

## What it does

**Proxy rotation.** Round-robin over the list you configure. A proxy that fails to
connect, times out, drops the connection or answers `407` to a plain `http://`
request it forwards itself goes on cooldown and is skipped until the cooldown
expires or the proxy answers again. A `Retry-After` on that `407` is deliberately
not honoured, since the wait belongs to the failed proxy, not to the server. A
per-attempt timeout counts as the proxy's failure, since the client cannot tell a
stalled proxy from a stalled origin; blaming it is cheap: the mark clears the first
time the proxy answers again.

If another proxy is out of cooldown the retry goes through it immediately; if none
is, retries fall back to the backoff. Any other status is the origin's answer: you
get it back, and the proxy stays healthy. Against an `https://` target, a refused
`CONNECT` never shows up as that `407`: it surfaces as a connect error instead,
which also puts the proxy on cooldown and is retried for every request.

Only the proxies you configure are used. `HTTP_PROXY` and friends are ignored.
`http://`, `https://` and bare `host:port` (treated as `http://host:port`) work out
of the box. `socks5://` and friends need the `socks` feature; without it they are
rejected when the client is built, not silently on every request. Proxy credentials
never reach `Debug` output, error messages, `tracing` events, or the `proxy` field
of an `on_retry` event, and logged URLs drop their query string, so a token passed as
a query parameter stays out of the log too. A header you add yourself through
`configure(|b| b.default_headers(..))` is not redacted this way: it shows up in
`Debug` output exactly as it would on a plain `reqwest::Client`. Mark its
`HeaderValue` sensitive with `set_sensitive(true)` if it needs to be.

**Per-host rate limiting.** A minimum interval between requests to the same host
name; port and scheme are not part of the key, so `http://` and `https://` traffic
to one host shares a single schedule. Enforced with `tokio::time`. Concurrent
callers to one host are serialised, not dropped.

**Retry with backoff.** `408`, `429` and `503` are retried for every request. Other
`5xx` (except `501`/`505`), request timeouts and connections dropped before a response
arrived (the classic keep-alive race of long-running scrapers) are retried for
idempotent methods only (`GET`, `HEAD`, `OPTIONS`, `PUT`, `DELETE`, `TRACE`), so a
`POST` is never duplicated. Failures that prove the server never acted on the request
are retried for everything: a `407` from a proxy (the proxy never forwarded it);
connect failures, including connect timeouts; requests cancelled before dispatch;
HTTP/2 `REFUSED_STREAM`; and a graceful HTTP/2 `GOAWAY(NO_ERROR)` that left the
request unprocessed. A `GOAWAY` naming an actual error code is idempotent-only, like
a stream reset.

reqwest's own retry layer is switched off, so `retries()` counts attempts exactly,
unless `configure(..)` sets a policy of its own. With `switch_proxy_on_429(true)`, a
`429` that came through a proxy is retried at once through a proxy this call has not
already seen limited, instead of waiting on its `Retry-After`, since a per-IP limit
does not bind another IP. The limited proxy is not put in cooldown. Once every
healthy proxy has answered `429` to this call, the usual `Retry-After`/backoff path
applies. Off by default.

Delays are full-jitter exponential with a configurable cap. A `Retry-After` header
(seconds or HTTP-date) can lengthen the wait but never shortens it, so `Retry-After: 0`
gets the same backoff as no header at all. Ask for longer than `max_retry_after`
(30 s by default) and you get the response instead of an early retry.

Before a retry, up to 64 KiB of an HTTP/1 error body is read so the connection can be
reused, for at most the coming backoff or 250 ms, whichever is longer; the read and
the backoff overlap rather than adding up. A body that does not end in that time or
within 64 KiB is dropped along with its connection, and one that declares a length
over 64 KiB is dropped without being read. Over HTTP/2 the body is never read:
dropping it resets only its own stream. After the last of N+1 attempts you get the
response as-is: check `status()`, or call `error_for_status()`, exactly as with
`reqwest`. A transport error is returned as `Error::Reqwest`; its message is the
short `request failed`, and the cause is the error's `source`, which `anyhow`'s
`{:#}` and most reporters print for you.

**Timeouts by default.** A bare `reqwest::Client` has none by default. This one has
both: 30 s per attempt, 10 s to connect, so a proxy that black-holes connections
cannot hang a request forever. Both are configurable. They bound one attempt, not
the whole call; wrap it in `tokio::time::timeout` for a hard overall budget. One trap
for tests using `#[tokio::test(start_paused = true)]`: tokio's auto-advancing clock
fires the request timeout the moment a task blocks on real socket I/O. Use real time
against a local server.

**Not just GET.** `request()` returns a builder wrapping `reqwest::RequestBuilder`;
calling `.send()` on it routes the request through the same rate limiting, rotation
and retries as `get()`. (Calling `.send()` on a plain `reqwest::RequestBuilder` sends
directly, with none of that: `request()` hands back a wrapper precisely so that
mistake doesn't compile silently into a scraper that leaks its real IP.) `send()` on
the client itself still takes a plain `reqwest::RequestBuilder`, for one built some
other way. `execute()` takes a pre-built `reqwest::Request`. A streaming body that
cannot be cloned is sent once, without retries.

**Yours to tune.** `configure(|builder| ...)` applies any `reqwest::ClientBuilder`
setting (default headers, redirect policy, TLS) to every underlying client. Features
such as `gzip`, `brotli` or `cookies` are enabled on your own `reqwest` dependency, as
usual. `query`, `form`, `json` and `multipart` are this crate's own features, off by
default, each enabling the matching `RequestBuilder` method.

`RotatingClient` is `Clone` (cheap; clones share pools, cooldowns and the rate
limiter), `Send + Sync`, `#![forbid(unsafe_code)]`, TLS via `rustls` by default;
disable default features and enable `native-tls` instead to use your platform's own
TLS. Clones of a `RotatingClientBuilder` share a pool given through `proxy_list`, so
every client built from them shares its cooldowns; see the builder for the details.

An optional `tracing` feature, off by default, logs retries and proxy rotation
at debug level; `on_retry(|event| ...)` reports each retry to a callback with the
failed attempt, the reason, the proxy and the delay, with or without that feature,
and both hide proxy credentials the same way. `examples/retry_metrics.rs` counts
retries by reason with it.

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

Licensed under either of [Apache License, Version 2.0][apache] or
[MIT license][mit] at your option.

[apache]: https://github.com/sopilnik/reqwest-rotate/blob/main/LICENSE-APACHE
[mit]: https://github.com/sopilnik/reqwest-rotate/blob/main/LICENSE-MIT
