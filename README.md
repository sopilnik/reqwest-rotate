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
cargo add tokio --features macros,rt-multi-thread
```

or add them to `Cargo.toml` directly:

```toml
[dependencies]
reqwest-rotate = "0.2"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
# Only if you name reqwest types yourself (configure, request, send, Url):
# it must be the same 0.13 this crate uses.
reqwest = { version = "0.13", default-features = false }
```

TLS defaults to `rustls`, which checks certificates against the operating
system's trust store: on Linux, install `ca-certificates` in a container
image. To use your platform's own TLS library instead, turn off default
features and enable `native-tls`:

```toml
[dependencies]
reqwest-rotate = { version = "0.2", default-features = false, features = ["native-tls"] }
```

### Features

| Feature | Default | Enables |
|---|---|---|
| `rustls` | on | TLS via rustls, OS trust store (`rustls-tls` is the 0.1 name) |
| `native-tls` | off | TLS via your platform's library instead; use with `default-features = false` |
| `socks` | off | `socks4://`, `socks4a://`, `socks5://`, `socks5h://` proxies |
| `json` | off | `RequestBuilder::json` |
| `form` | off | `RequestBuilder::form` |
| `query` | off | `RequestBuilder::query` |
| `multipart` | off | `RequestBuilder::multipart` |
| `tracing` | off | debug-level events for retries and proxy rotation |

`gzip`, `brotli`, `cookies` and other `reqwest` features are enabled on
your own `reqwest` dependency.

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

[examples] has three more: `get`, `proxy_pool` and `retry_metrics`.

## What it does

**Proxy rotation.** Round-robin over the list you configure. A proxy that fails to
connect, times out, drops the connection or answers `407` to a plain `http://`
request it forwards itself goes on cooldown and is skipped until the cooldown
expires or the proxy answers again. A `Retry-After` on that `407` is deliberately
not honoured, since the wait belongs to the failed proxy, not to the server. A
per-attempt timeout counts as the proxy's failure too: the client cannot tell a
stalled proxy from a stalled origin, and blaming it is cheap, since the mark clears
the first time the proxy answers again.

If another proxy is out of cooldown the retry goes through it immediately; if none
is, retries fall back to the backoff. Any other status is the origin's answer: you
get it back, and the proxy stays healthy. Against an `https://` target, a refused
`CONNECT` never shows up as that `407`: it surfaces as a connect error instead,
which also puts the proxy on cooldown and is retried for every request.

A failed TLS handshake with the origin is a connect failure too, a certificate that
does not verify included: it is retried like one, and through a proxy it puts that
proxy on cooldown, since a proxy that intercepts TLS fails in exactly this way. An
origin with a broken certificate therefore uses up every attempt, and can cool down
one proxy per attempt.

Only the proxies you configure are used; `HTTP_PROXY` and friends are ignored.
`http://`, `https://` and bare `host:port` work out of the box. SOCKS schemes need
the `socks` feature; without it `build()` rejects them, instead of every request
failing later. A `host:port:user:pass` line, as vendors export it, is rejected with
a hint to write `http://user:pass@host:port`. An `http://` or SOCKS proxy gets your
credentials in the clear, so use `https://` if you do not trust the path to it.

With proxies set, nothing goes out directly. If every proxy is cooling down, the one
that comes back first takes the traffic, so one slow origin can pile everything onto
it for up to `proxy_cooldown`. [`ProxyList`] (from `client.proxies()`) shows the
cooldowns, and `mark_bad`/`mark_good` let your own health check steer it.

Proxy credentials never reach `Debug` output, error messages, `tracing` events
or the `proxy` field of an [`on_retry`] event, and logged URLs drop their query
string too, so a token passed as a query parameter stays out of the log as
well. A header you add yourself through `configure(|b| b.default_headers(..))`
is not redacted this way: it shows up in `Debug` output exactly as it would on
a plain `reqwest::Client`. Mark its `HeaderValue` sensitive with
`set_sensitive(true)` if it needs to be.

**Per-host rate limiting.** A minimum interval between requests to the same host
name; port and scheme are not part of the key, so `http://` and `https://` traffic
to one host shares a single schedule. Enforced with `tokio::time`. Concurrent
callers to one host are serialised, not dropped.

**Retry with backoff.**

| Retried for | When |
|---|---|
| any method, `POST` too | `408`, `429`, `503`; a `407` from a proxy that forwarded the request itself; connect failures, connect timeouts and failed TLS handshakes included; requests cancelled before dispatch; HTTP/2 `REFUSED_STREAM`, or a `GOAWAY(NO_ERROR)` that left the request unprocessed |
| `GET`, `HEAD`, `OPTIONS`, `PUT`, `DELETE`, `TRACE` | other `5xx` except `501`/`505`; request timeouts; any other HTTP/2 reset or error-code `GOAWAY`; a connection dropped before the response (the classic keep-alive race of long-running scrapers) |

Nothing else is retried. In the first row the server never acted on the request,
so a `POST` is never sent twice.

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

**Not just GET.** [`RotatingClient::request`] returns a builder wrapping
`reqwest::RequestBuilder`; calling `.send()` on it routes the request through
the same rate limiting, rotation and retries as `get()`. (Calling `.send()` on
a plain `reqwest::RequestBuilder` sends directly, with none of that:
`request()` hands back a wrapper precisely so that mistake does not compile
silently into a scraper that leaks its real IP.) `send()` on the client itself
still takes a plain `reqwest::RequestBuilder`, for one built some other way.
`execute()` takes a pre-built `reqwest::Request`. A streaming body that cannot
be cloned is sent once, without retries.

**Yours to tune.** [`configure`]`(|builder| ...)` applies any
`reqwest::ClientBuilder` setting (default headers, redirect policy, TLS) to
every underlying client; the Features table above says which features are
yours to enable. Each underlying client keeps its own cookie store and its
own HTTP/2 connections, so `cookie_store(true)` here gives every proxy a
separate jar: share one `Arc<reqwest::cookie::Jar>` with
`configure(move |b| b.cookie_provider(Arc::clone(&jar)))` if a session
needs to survive rotation.

`RotatingClient` is `Clone` (cheap; clones share pools, cooldowns and the rate
limiter), `Send + Sync` and `#![forbid(unsafe_code)]`. Clones of a
`RotatingClientBuilder` share a pool given through `proxy_list`, so every
client built from them shares its cooldowns; see the builder for the details.

The `tracing` feature logs retries and proxy rotation at debug level;
[`on_retry`]`(|event| ...)` reports each retry to a callback with the failed
attempt, the reason, the proxy and the delay, with or without that feature,
and both hide proxy credentials the same way. `examples/retry_metrics.rs`
counts retries by reason with it.

## Why not `reqwest-proxy-pool` or `reqwest-retry`?

`reqwest-proxy-pool` is a proxy pool *middleware*: it manages proxy
selection as a layer you plug into your own client stack. `reqwest-rotate`
is the opposite trade-off: one ready-to-use client that bundles rotation,
rate limiting, and retry together, for when you want a working scraper
client and do not want to assemble the pieces yourself.

`reqwest-retry` (on `reqwest-middleware`) does retries, but knows nothing about
proxies or per-host pacing: a retry goes out the same way the failed attempt did.
If you already run a middleware stack, it is the smaller step. If you want a retry
that knows which proxy failed, that is what this crate is for.

## Minimum supported Rust version

MSRV is 1.85 (edition 2024), checked in CI by `cargo check --all-features`
on that toolchain. The library only: the dev-dependencies need a newer
compiler.

## License

Licensed under either of [Apache License, Version 2.0][apache] or
[MIT license][mit] at your option.

[apache]: https://github.com/sopilnik/reqwest-rotate/blob/main/LICENSE-APACHE
[mit]: https://github.com/sopilnik/reqwest-rotate/blob/main/LICENSE-MIT
[`on_retry`]: https://docs.rs/reqwest-rotate/latest/reqwest_rotate/struct.RotatingClientBuilder.html#method.on_retry
[`configure`]: https://docs.rs/reqwest-rotate/latest/reqwest_rotate/struct.RotatingClientBuilder.html#method.configure
[`RotatingClient::request`]: https://docs.rs/reqwest-rotate/latest/reqwest_rotate/struct.RotatingClient.html#method.request
[`ProxyList`]: https://docs.rs/reqwest-rotate/latest/reqwest_rotate/struct.ProxyList.html
[examples]: https://github.com/sopilnik/reqwest-rotate/tree/main/examples
