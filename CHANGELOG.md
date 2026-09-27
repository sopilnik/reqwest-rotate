# Changelog

All notable changes to this crate are documented in this file. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `ProxyList::mark_good`. Takes a proxy out of cooldown early: your own
  health check saw it answer, or you put it there with `mark_bad`.
- `ProxyList::iter`. Yields the configured proxies as an iterator
  instead of a slice, so code that reads the list no longer depends
  on how it is stored. `as_slice` stays available until 1.0.
- `RotatingClientBuilder` is now `Clone`. A pool given through
  `proxy_list` and a hook given through `configure` are shared
  between clones, cooldowns included; a pool given as plain URLs
  through `proxies` is built fresh by each `build()` and is not
  shared.
- `RotatingClientBuilder::switch_proxy_on_429`. When on, a `429` that
  came through a proxy is retried at once through a proxy this call
  has not already seen limited, instead of waiting: a per-IP rate
  limit does not bind another IP. The `429`'s `Retry-After` is
  ignored and the limited proxy is not put in cooldown. Once every
  healthy proxy has answered `429` to a call, the usual
  `Retry-After`/backoff path applies. Off by default.
- `RotatingClientBuilder::on_retry`: a callback before each retry
  with the attempt, the reason, the (redacted) proxy and the delay.
  Handy for metrics without a tracing subscriber.
- Three runnable examples under `examples/`: `get` fetches one URL
  with retries and backoff, `proxy_pool` rotates across proxies read
  from `PROXIES` and reports their cooldown state, and `retry_metrics`
  counts retries by reason with `on_retry`.
- `ProxyList::iter_redacted`. Yields the configured proxies with any
  `user:password@` replaced by `***@`, the same redaction `Debug`
  output and `tracing` events use, so a caller no longer has to strip
  credentials by hand before logging or printing one.
- `RetryReason::as_str`. A short, stable label (`status`,
  `proxy_status`, `timeout`, `connect`, `never_sent`, `transport`)
  for metrics, in place of formatting the `Debug` output by hand.
- `RetryEvent::host`. The host the failed attempt was sent to, so a
  per-host retry count no longer needs the request URL threaded
  through some other way.

### Changed

- An HTTP/2 `GOAWAY(NO_ERROR)` that left a request unprocessed is
  now retried for every method, including `POST`, like
  `REFUSED_STREAM`. A `GOAWAY` naming an actual error code is
  unchanged: idempotent methods only.
- `reqwest` is now 0.13, up from 0.12. This crate's API takes and
  returns `reqwest` types (`reqwest::Response`, `reqwest::Request`,
  `reqwest::RequestBuilder`, the `reqwest::ClientBuilder` that
  `configure` hands you, and `reqwest::Error` inside `Error`), so a
  project that also depends on `reqwest` directly needs the same
  version in both places.
- The `rustls-tls` feature is renamed `rustls`, matching reqwest's
  own name for it. `rustls-tls` stays as an alias, so an existing
  `Cargo.toml` naming it still builds.
- `RequestBuilder::query` and `RequestBuilder::form` move behind
  their own `query` and `form` features, the same way `json` and
  `multipart` already do.
- reqwest 0.13's `rustls` feature builds on `aws-lc-rs` instead of
  `ring`, and verifies certificates against the operating system's
  own trust store instead of the bundled `webpki-roots` list. On
  Linux that is the system CA bundle: without one (no
  `ca-certificates` package, no `SSL_CERT_FILE`),
  `RotatingClientBuilder::build` now returns `Error::Build`.
- With `native-tls`, HTTPS connections now offer HTTP/2 through
  ALPN and use it when the server agrees, as `rustls` already did:
  reqwest 0.13 folded its `native-tls-alpn` feature into
  `native-tls`. Before, `native-tls` stayed on HTTP/1.1.
- The pre-retry body drain now runs inside the coming backoff instead
  of before it, and gives up after that backoff or 250 ms, whichever
  is longer. An error body that does not end in time now costs a
  reconnect instead of stalling the retry, and a body that declares
  more than 64 KiB, or arrives over HTTP/2, is no longer read at all.
- Each per-proxy client now caps its idle connections at 15 s and 8
  per host, instead of reqwest's own 90 s and unbounded defaults.
  Round-robin spreads one host's traffic across every proxy, so the
  old defaults could leave far more idle sockets open than a plain
  client would for the same traffic; `configure` still overrides
  either value.
- Requests queued for a rate-limited host now leave the configured
  interval apart even when a busy runtime wakes them late. Before,
  every overdue request for that host went out in the same tick.
- The docs.rs front page is now `README.md`.
- docs.rs now marks `RequestBuilder::query`, `form`, `json` and
  `multipart` with the feature each one needs.
- Minimum dependency versions raised to ones that actually resolve
  together with reqwest 0.13: `http` 1.1, `hyper` 1.6.0, `h2` 0.4.2,
  `tracing` 0.1.35, `serde` 1.0.220, `thiserror` 2.0.3, `tokio`
  1.28.1.
- `Error::InvalidProxy`'s own message no longer repeats its reason;
  read the reason from `source()`, as for the other variants, so a
  chain-walking reporter such as `anyhow`'s `{:#}` prints it once.
- `ProxyList::in_cooldown` is now `#[must_use]`, like the other queries
  on `ProxyList`.
- The minimum supported Rust version stays 1.85.

### Fixed

- A `Retry-After` header can no longer shorten the wait below the
  computed backoff, only lengthen it. Before, a server answering `429`
  with `Retry-After: 0` got every remaining retry back to back with no
  backoff at all.
- `retries(n)` is now exact. reqwest's own retry layer is switched
  off on every client this crate builds. Before, reqwest could
  resend an HTTP/2 request after `REFUSED_STREAM` or
  `GOAWAY(NO_ERROR)` up to twice under each attempt, and replay a
  `POST` with a clonable body by its own rules instead of this
  crate's.
- A `host:port:user:pass` proxy line, as many vendors export it, is
  now rejected with a reason naming the accepted spelling instead of
  a bare "not a valid proxy URL", and without its password in the
  error. A blank entry now names itself instead of leaving the proxy
  field empty.
- A `Retry-After` delta-seconds value with more digits than a `u64`
  holds is now treated as longer than any cap, and the response is
  returned instead of retrying early. Before, a value that large
  failed to parse and fell back to the backoff, as if the header had
  asked for nothing at all.
- A request-body stream that fails on the caller's own side no longer
  cools down the proxy that carried it. The source walk that decides
  whether a proxy is to blame now stops at the first `hyper` error in
  the chain instead of continuing past it to an `io::Error` from the
  caller's own stream.
- A `407` reaching the client through a `CONNECT` tunnel or a SOCKS
  proxy is now treated as the origin's own answer, not the proxy's:
  only a proxy that forwards a plain `http://` request itself can
  produce that status. Before, any `407` seen on an attempt through a
  proxy was blamed on the proxy, which could replay a `POST` the
  origin had already received and cool down healthy proxies for an
  answer they never gave.

### Migrating from 0.1

- Move your own `reqwest` dependency to 0.13.
- `rustls-tls` still works; rename it to `rustls` only if you want
  to match reqwest's own name.
- Enable the `query` or `form` feature if you call
  `RequestBuilder::query` or `RequestBuilder::form`.
- On Linux, give the machine or container a CA bundle (the
  `ca-certificates` package, or `SSL_CERT_FILE` pointing at one):
  the default `rustls` build no longer carries its own.
- If you counted on reqwest replaying an HTTP/2 `REFUSED_STREAM` or
  `GOAWAY(NO_ERROR)` underneath `retries(n)`, those attempts are now
  this crate's own: raise `retries` if a low count starts running out.
- A `Retry-After` shorter than the computed backoff, `0` included, no
  longer cuts the wait short. Nothing to change unless a test of yours
  timed a `Retry-After: 0` retry.
- If you print `Error::InvalidProxy` with `{}` and want to see why
  the proxy was rejected, print its `source()` too, or use a reporter
  that walks the chain, such as `anyhow`'s `{:#}`.
- With `native-tls`, HTTPS now uses HTTP/2 when the server offers it.
  To stay on HTTP/1.1, call `configure(|b| b.http1_only())`.

## [0.1.1] - 2026-09-16

### Changed

- The repository now lives at `github.com/sopilnik/reqwest-rotate`
  after the GitHub account was renamed. The crate metadata, the CI
  badge and the changelog links point there. No code changes.

## [0.1.0] - 2026-09-09

First release.

`RotatingClient` wraps `reqwest` with the three things almost every
scraper or API client ends up building for itself:

- **Proxy rotation.** Configure a pool of proxies and requests go out
  round-robin across them. A proxy that fails to connect, times out,
  drops the connection or answers `407 Proxy Authentication Required`
  is put on cooldown and skipped until it recovers or the cooldown
  expires, whichever comes first.
- **Per-host rate limiting.** Set a minimum interval between requests
  to the same host; concurrent callers are queued, not dropped.
- **Retry with backoff.** Retryable statuses and transport failures
  are retried automatically with full-jitter exponential backoff, and
  a server's own `Retry-After` header is honoured when present.

Also in this release:

- TLS is a choice, not a given: the `rustls-tls` feature is on by
  default, and a `native-tls` feature is available for projects that
  want their platform's own TLS library instead (`default-features =
  false`, `features = ["native-tls"]`).
- Optional features for the rest: `json` and `multipart` forward to
  the matching `reqwest` features, `socks` enables `socks4://`,
  `socks4a://`, `socks5://` and `socks5h://` proxies, and `tracing`
  emits debug-level events for retries and proxy rotation.

[Unreleased]: https://github.com/sopilnik/reqwest-rotate/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/sopilnik/reqwest-rotate/releases/tag/v0.1.1
[0.1.0]: https://github.com/sopilnik/reqwest-rotate/releases/tag/v0.1.0
