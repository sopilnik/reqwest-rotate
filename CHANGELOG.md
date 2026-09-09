# Changelog

All notable changes to this crate are documented in this file. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project uses [Semantic Versioning](https://semver.org/).

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

[0.1.0]: https://github.com/KsandrKj/reqwest-rotate/releases/tag/v0.1.0
