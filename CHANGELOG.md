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
  came through a proxy is retried at once through the next proxy in
  rotation instead of waiting: a per-IP rate limit does not bind
  another IP. The `429`'s `Retry-After` is ignored and the limited
  proxy is not put in cooldown. Off by default.
- `RotatingClientBuilder::on_retry`. Runs a callback before each retry
  with the attempt number, why it failed, the proxy in use (redacted
  the same way as `tracing` output) and the coming delay, for counters
  and metrics that do not want a tracing subscriber. `RetryEvent` and
  `RetryReason` are `#[non_exhaustive]`, so more fields and variants
  can be added later without a breaking change. `RetryReason` also
  implements `Hash` and `Ord`, so it can key a map.
- Three runnable examples under `examples/`: `get` fetches one URL
  with retries and backoff, `proxy_pool` rotates across proxies read
  from `PROXIES` and reports their cooldown state, and `retry_metrics`
  counts retries by reason with `on_retry`.

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
- The crate documentation on docs.rs is now generated from
  `README.md` instead of a separate copy in `src/lib.rs`, so there
  is one copy of it to keep current instead of two that had already
  drifted apart.
- docs.rs now marks `RequestBuilder::query`, `form`, `json` and
  `multipart` with the feature each one needs.
- Minimum dependency versions raised to what the rest of the
  dependency graph already requires, so `Cargo.toml` no longer
  allows releases that cannot be resolved together: `http` 1.1,
  `hyper` 1.6.0, `h2` 0.4.2 and `tracing` 0.1.35 (reqwest 0.13 and
  its HTTP stack), `serde` 1.0.220 (the first release that shares
  its traits with current `serde_json`), `thiserror` 2.0.3 and
  `tokio` 1.28.1 (reqwest's optional HTTP/3 dependency `quinn`,
  which Cargo resolves even though this crate never enables it).

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
