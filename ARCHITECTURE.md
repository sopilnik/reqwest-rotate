# Architecture

`reqwest-rotate` is one library crate that wraps `reqwest` with proxy rotation, per-host rate limiting and
retry with backoff. The README says what it does for a user; this file says where things live and what has to stay true
when the code changes. Decisions that explain a rule are in `docs/decisions/`, the dependency ledger is
`docs/dependencies.md`.

## Module graph

<!-- Maintained by hand; no tool generates it. An edge is a `use crate::...`, a `crate::` path or one of the
`pub use` lines of `lib.rs`, in non-test code, that reaches another module of `src/`. The `mod` declarations, links in
doc comments and the test modules are not edges. -->

```text
lib        ──► client, error, proxy, retry
client     ──► error, proxy, rate_limit, retry, lib
proxy      ──► error, lib
rate_limit ──► lib
error, retry: no edges out
```

The edges out of `lib` are its `pub use` lines. The edges into `lib` are `MAX_DURATION` (used by `client`, `proxy` and
`rate_limit`) and the `trace_log!` macro (used by `client`). `lib.rs` declares all five modules and re-exports the
public items (`RotatingClient`, `RotatingClientBuilder`, `RequestBuilder`, `ProxyList`, `Error`, `RetryEvent`,
`RetryReason`). It holds no logic beyond the `MAX_DURATION` constant and the `trace_log!` macro. `rate_limit` is private
to the crate and has no `pub use`; `retry` is private except for the two event types it defines.

## Where a new piece goes

- A new knob on the client: a field on `RotatingClientBuilder` and on `Inner` in `src/client.rs`, clamped to
  `MAX_DURATION` if it feeds `Instant` arithmetic (see the invariants below), with a test in `tests/` for the
  behaviour and a line in `CHANGELOG.md`.
- A new rule for what is retried: a pure function in `src/retry.rs` that takes a status, a method or an error and
  answers yes or no; `client.rs` only calls it.
- A new proxy scheme or a new check of the proxy list: `src/proxy.rs`, inside `canonical_proxy_url` or `check_scheme`.
- A new public error case: a variant of `Error` in `src/error.rs`. The enum is `#[non_exhaustive]`.
- A new reqwest feature the crate passes through: a feature in `Cargo.toml` that enables the same feature of `reqwest`,
  and a row in the README feature table.
- A new example: a file in `examples/`; it must build with the default features.
- A new fuzz target: a file in `fuzz/fuzz_targets/` and a `[[bin]]` entry in `fuzz/Cargo.toml`. The `fuzz/` crate is a
  workspace of its own, outside the library's build, and is not packaged (`include` in `Cargo.toml` leaves it out).

## Invariants that span modules

- Nothing holds a lock across an `.await`. `ProxyList` and `RateLimiter` keep their state behind a `Mutex` and release
  it before any wait.
- The durations that feed `Instant` arithmetic cannot overflow it: the backoff base and cap, `max_retry_after` and the
  rate-limit interval are clamped to `MAX_DURATION` when the client is built, and a proxy cooldown that would overflow
  an `Instant` is capped at `MAX_DURATION` when it is applied. The two timeouts are handed to `reqwest` as given; the
  docs of `timeout` say that `Duration::MAX` effectively disables it.
- A request sent through the crate (`get`, `send`, `execute`, or `send` on the crate's `RequestBuilder`) goes through
  the rate limiter first, then picks a proxy, then sends. `RequestBuilder::into_inner` and `RequestBuilder::build` hand
  back plain `reqwest` objects; sending those on a plain `reqwest` client skips rate limiting, rotation and retries. The
  docs of `into_inner` say so for the builder it returns.
- Only the configured proxies are used. Each underlying client is built with `no_proxy()` unless it has one of the
  configured proxies, and `reqwest`'s own retry layer is switched off, so `retries()` counts attempts exactly. The
  `configure(..)` closure runs after those settings and wins, and it gets the whole `reqwest::ClientBuilder`. A
  `.retry(..)` there brings `reqwest`'s retries back, and `pool_idle_timeout` / `pool_max_idle_per_host` there change
  the pool settings (the docs of `configure` and of `proxies` say so).
- A request is retried only when the classification in `src/retry.rs` allows it: a non-idempotent method is replayed
  only when the server provably never acted on the request. A body that cannot be cloned is sent once.
- Proxy credentials are redacted wherever the crate itself shows a proxy: `Debug` output, error messages,
  `tracing` events, `RetryEvent::proxy`. The accessors that return the stored URL are the exception (see the classes
  of data). New code that prints a proxy or a URL goes through `redact_userinfo` / `ProxyList::redacted` / `log_url`.
- The library contains no `unsafe` (decision 0004). A URL without a host is keyed by the empty string in the rate
  limiter (decision 0005).

## Security

The crate is a client library: it listens on no port and has no users or roles of its own, so it has no access matrix.

**Trust boundaries.**
- The caller is trusted to supply the proxy list, the request and the configuration; the list is validated and
  canonicalised when it is built, and an invalid entry is an error at `build()`.
- Everything that comes back over the network is untrusted: response status, headers (`Retry-After` is parsed, and a
  wait longer than `max_retry_after` is not taken), and bodies. At most about 64 KiB of an HTTP/1 body is read before a
  retry (nothing when its declared length is larger), and none of an HTTP/2 body.
- A proxy is trusted with the traffic it is given, and with the credentials in its URL; a plain `http://` or SOCKS proxy
  sees them in the clear.
- The process environment is not trusted: `HTTP_PROXY` and the other proxy variables are ignored.

**Public entry points.** The items re-exported from `src/lib.rs`:
- `RotatingClient`: `builder`, `get`, `request`, `send`, `execute`, `proxies`.
- `RotatingClientBuilder`: `proxies`, `proxy_list`, `rate_limit`, `retries`, `backoff`, `max_retry_after`,
  `proxy_cooldown`, `switch_proxy_on_429`, `user_agent`, `timeout`, `connect_timeout`, `configure`, `on_retry`, `build`.
- `RequestBuilder`: `header`, `headers`, `basic_auth`, `bearer_auth`, `body`, `timeout`, `version`, `build`, `send`,
  `into_inner`. `build` gives a plain `reqwest::Request` and `into_inner` a plain `reqwest::RequestBuilder`.
- `ProxyList`: `new`, `is_empty`, `len`, `as_slice`, `iter`, `iter_redacted`, `pick`, `mark_bad`, `mark_good`,
  `in_cooldown`.
- `Error`: the variants `Reqwest`, `InvalidProxy` and `Build`; it has no methods of its own.
- `RetryEvent`: the public fields `attempt`, `reason`, `proxy`, `host` and `delay`.
- `RetryReason`: the variants `Status`, `ProxyStatus`, `Timeout`, `Connect`, `NeverSent` and `Transport`, and
  `as_str`.

Cargo features change what is exposed: `query`, `form`, `json` and `multipart` add the `RequestBuilder` method of the
same name, `socks` adds the `socks4://`, `socks4a://`, `socks5://` and `socks5h://` proxy schemes, and `native-tls`
swaps the TLS library.

**Classes of data.**
- Proxy credentials (the userinfo of a proxy URL): redacted in `Debug` output, error messages, `tracing` events and
  `RetryEvent::proxy`. `ProxyList::pick`, `ProxyList::as_slice` and `ProxyList::iter` hand the canonical URLs back to
  the caller with the credentials, since that is what the client connects with, and their docs say to strip them before
  logging; `ProxyList::iter_redacted` is the form that is safe to print.
- Request URLs: logged without the query string, because a token can travel there. The `reqwest::Error` in an error
  chain prints the full URL, which the `Error` documentation says.
- Headers the caller adds through `configure(..)`: not redacted unless the caller marks the value sensitive.
- Request and response bodies: passed through and never logged.

**Secrets.** The crate stores no secret of its own, and the repository holds none either. CI uses only the workflow's
own `GITHUB_TOKEN`. It is read-only everywhere except the Scorecard and CodeQL jobs, which may also upload code-scanning
results (`security-events: write`).
