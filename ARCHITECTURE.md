# Architecture

`reqwest-rotate` is one library crate that wraps `reqwest` with proxy rotation, per-host rate limiting and
retry with backoff. The README says what it does for a user; this file says where things live and what has to stay true
when the code changes. Decisions that explain a rule are in `docs/decisions/`, the dependency ledger is
`docs/dependencies.md`.

## Module graph

<!-- Kept by hand: no standard command is run for it. An edge is a `use crate::...` between modules of `src/`. -->

```text
lib ──► client ──► error
 │        ├──────► proxy ──► error
 │        ├──────► rate_limit
 │        └──────► retry
 └─ owns `MAX_DURATION` (used by client, proxy and rate_limit) and `trace_log!` (used by client)
```

`lib.rs` declares the modules and re-exports the public items (`RotatingClient`, `RotatingClientBuilder`,
`RequestBuilder`, `ProxyList`, `Error`, `RetryEvent`, `RetryReason`). It holds no logic beyond the `MAX_DURATION`
constant and the `trace_log!` macro. `rate_limit` is private to the crate; `retry` is private except for the two event
types it defines.

## Where a new piece goes

- A new knob on the client: a field on `RotatingClientBuilder` and on `Inner` in `src/client.rs`, clamped to
  `MAX_DURATION` if it is a duration, with a test in `tests/` for the behaviour and a line in `CHANGELOG.md`.
- A new rule for what is retried: a pure function in `src/retry.rs` that takes a status, a method or an error and
  answers yes or no; `client.rs` only calls it.
- A new proxy scheme or a new check of the proxy list: `src/proxy.rs`, inside `canonical_proxy_url` or `check_scheme`.
- A new public error case: a variant of `Error` in `src/error.rs`. The enum is `#[non_exhaustive]`.
- A new reqwest feature the crate passes through: a feature in `Cargo.toml` that enables the same feature of `reqwest`,
  and a row in the README feature table.
- A new example: a file in `examples/`; it must build with the default features.

## Invariants that span modules

- Nothing holds a lock across an `.await`. `ProxyList` and `RateLimiter` keep their state behind a `Mutex` and release
  it before any wait.
- The durations that feed `Instant` arithmetic cannot overflow it: the backoff base and cap, `max_retry_after` and the
  rate-limit interval are clamped to `MAX_DURATION` when the client is built, and a proxy cooldown that would overflow
  an `Instant` is capped at `MAX_DURATION` when it is applied. The two timeouts are handed to `reqwest` as given, and
  `Duration::MAX` effectively turns them off, as the builder's docs say.
- A request sent through the crate (`get`, `send`, `execute`, or `send` on the crate's `RequestBuilder`) goes through
  the rate limiter first, then picks a proxy, then sends. `RequestBuilder::into_inner` and `RequestBuilder::build` hand
  back plain `reqwest` objects; sending those directly skips rate limiting, rotation and retries, and the docs of
  `into_inner` say so.
- Only the configured proxies are used. Each underlying client is built with `no_proxy()` unless it has one of the
  configured proxies, and `reqwest`'s own retry layer is switched off, so `retries()` counts attempts exactly. The
  `configure(..)` closure runs after those settings and wins: it can set a proxy, bring `reqwest`'s retries back or
  change the pool settings, and the docs of `configure` say so.
- A request is retried only when the classification in `src/retry.rs` allows it: a non-idempotent method is replayed
  only when the server provably never acted on the request. A body that cannot be cloned is sent once.
- Proxy credentials are redacted at every exit: `Debug` output, error messages, `tracing` events, `RetryEvent::proxy`.
  New code that prints a proxy or a URL goes through `redact_userinfo` / `ProxyList::redacted` / `log_url`.
- The library contains no `unsafe` (decision 0004). A URL without a host is keyed by the empty string in the rate
  limiter (decision 0005).

## Security

The crate is a client library: it holds no secrets of its own, listens on no port and has no users of its own.

**Trust boundaries.**
- The caller is trusted to supply the proxy list, the request and the configuration; the list is validated and
  canonicalised when it is built, and an invalid entry is an error at `build()`.
- Everything that comes back over the network is untrusted: response status, headers (`Retry-After` is parsed, and a
  wait longer than `max_retry_after` is not taken), and bodies. Roughly 64 KiB of an HTTP/1 body is read before a
  retry, and none of an HTTP/2 body.
- A proxy is trusted with the traffic it is given, and with the credentials in its URL; a plain `http://` or SOCKS proxy
  sees them in the clear.
- The process environment is not trusted: `HTTP_PROXY` and the other proxy variables are ignored.

**Roles and access matrix.** None. The crate has no authentication and no roles, so there is no matrix to keep.

**Public entry points.** The items re-exported from `src/lib.rs`: `RotatingClient` (`get`, `request`, `send`, `execute`,
`proxies`), `RotatingClientBuilder`, `RequestBuilder`, `ProxyList` (`new`, `pick`, `mark_bad`, `mark_good`,
`in_cooldown`, the iterators), `Error`, `RetryEvent` and `RetryReason`. Cargo features change what is exposed:
`query`, `form`, `json` and `multipart` add builder methods, `socks` and `native-tls` change which proxies and which TLS
library are accepted.

**Classes of data.**
- Proxy credentials (the userinfo of a proxy URL): redacted everywhere they could be printed.
- Request URLs: logged without the query string, because a token can travel there. The `reqwest::Error` in an error
  chain prints the full URL, which the `Error` documentation says.
- Headers the caller adds through `configure(..)`: not redacted unless the caller marks the value sensitive.
- Request and response bodies: passed through and never logged.

**Secrets.** The crate stores none. Proxy credentials passed in by the caller live in memory in the `ProxyList` for the
life of the client and are never written out. The repository holds no secret; CI uses only the read-only token of the
workflow.
