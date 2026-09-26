//! `reqwest-rotate` bundles the three things almost every scraper or API
//! client needs on top of [`reqwest`]: rotating across a pool of proxies,
//! rate-limiting requests per host, and retrying transient failures with
//! backoff that honours `Retry-After`.
//!
//! It is deliberately small: one client, one builder, no middleware traits.
//! Proxies are optional. Without them [`RotatingClient`] is just a
//! rate-limited, retrying wrapper around `reqwest` with sane timeouts.
//!
//! # Example
//!
//! ```no_run
//! use reqwest_rotate::RotatingClient;
//! use std::time::Duration;
//!
//! # async fn run() -> Result<(), reqwest_rotate::Error> {
//! let client = RotatingClient::builder()
//!     .proxies([
//!         "http://user:pass@proxy1.example:8000",
//!         "http://user:pass@proxy2.example:8000",
//!     ])
//!     .rate_limit(Duration::from_millis(500))
//!     .retries(4)
//!     .backoff(Duration::from_millis(200), Duration::from_secs(30))
//!     .proxy_cooldown(Duration::from_secs(60))
//!     .timeout(Duration::from_secs(20))
//!     .user_agent("my-scraper/0.1")
//!     .build()?;
//!
//! let response = client.get("https://example.com/api").await?;
//! println!("status: {}", response.status());
//! # Ok(())
//! # }
//! ```
//!
//! # What gets retried
//!
//! - Responses with status 408, 429 or 503, for every request; other 5xx
//!   (except 501 and 505) only for idempotent requests, since a `POST` the
//!   server processed and then failed to answer would be duplicated.
//!   Backoff is full-jitter exponential. A `Retry-After` header can
//!   lengthen the wait but never shortens it: `Retry-After: 0` gets the
//!   same backoff as no header at all. If the server asks for a wait
//!   longer than `max_retry_after` (30 s by default), the response is
//!   returned right away instead of retrying early against its wishes.
//! - A `407` answered by a configured proxy, for every request including
//!   `POST`: the proxy did not forward anything, so nothing can be
//!   duplicated. The proxy is put in cooldown and the next attempt goes
//!   through another one; a `Retry-After` on that `407` is deliberately
//!   not honoured, since the wait belongs to the failed proxy, not to the
//!   server.
//! - A `429` that came through a proxy, when
//!   [`switch_proxy_on_429`](RotatingClientBuilder::switch_proxy_on_429)
//!   is on and another proxy is out of cooldown: retried at once through
//!   the next proxy in rotation instead of waiting, since a per-IP limit
//!   does not bind another IP. That `429`'s `Retry-After` is ignored and
//!   the limited proxy is not put in cooldown. Off by default.
//! - Transport errors that prove the server never acted on the request:
//!   connect failures (including connect timeouts), requests cancelled
//!   before dispatch, HTTP/2 `REFUSED_STREAM`, and a graceful HTTP/2
//!   `GOAWAY(NO_ERROR)` that left this stream unprocessed. Retried for
//!   every request.
//! - Other transport errors: a total-request timeout, a connection closed
//!   or reset before the response arrived (the classic keep-alive race of
//!   long-running scrapers), an HTTP/2 `GOAWAY` naming an actual error
//!   code, or a stream reset. Retried only for idempotent methods
//!   (`GET`, `HEAD`, `OPTIONS`, `PUT`, `DELETE`, `TRACE`).
//!
//! reqwest's own retry layer is switched off on every client this crate
//! builds, so `retries()` counts attempts exactly; pass a policy to
//! [`configure`](RotatingClientBuilder::configure) to bring it back.
//!
//! After the last attempt the response is returned as-is, whatever its
//! status, and a transport error is returned as [`Error::Reqwest`]. Its
//! message is the short `request failed`; the cause is the error's
//! [`source`](std::error::Error::source), which `anyhow`'s `{:#}` and most
//! reporters print for you. Each attempt is bounded by the timeouts below,
//! not the whole call; wrap the call in [`tokio::time::timeout`] for a
//! hard overall budget.
//!
//! # Proxies
//!
//! Proxies are used round-robin. A proxy that fails at the transport level
//! (connect failure, timeout, dropped or reset connection) or answers
//! `407 Proxy Authentication Required` is put on cooldown and skipped until
//! the cooldown expires, or until it answers a request again, whichever
//! comes first. A per-attempt timeout counts as the proxy's failure, since
//! the client cannot tell a stalled proxy from a stalled origin; blaming
//! it is cheap: the mark clears the first time the proxy answers again.
//! While another proxy is out of cooldown, the retry goes through it right
//! away; once no other proxy is available, retries are paced by the
//! backoff. Any other status is the origin's answer, and the proxy keeps
//! its place in the rotation. Against an `https://` target, a proxy that
//! refuses the `CONNECT` tunnel never gets to answer with a `407`. The
//! refusal surfaces as a connect error instead: the proxy goes on
//! cooldown, and the request is retried for every method, like any other
//! connect failure.
//!
//! Only proxies you configure are used: the `HTTP_PROXY`, `HTTPS_PROXY`
//! and `ALL_PROXY` environment variables are ignored. `http://` and
//! `https://` proxies work out of the box; `socks5://` and friends need
//! the `socks` cargo feature (without it they are rejected by `build()`
//! instead of silently misbehaving). A bare `host:port` is accepted and
//! treated as `http://host:port`. Proxy credentials are hidden from
//! `Debug` output, error messages, `tracing` events, and the `proxy`
//! field of an [`on_retry`](RotatingClientBuilder::on_retry) event. A
//! header added through [`configure`](RotatingClientBuilder::configure)'s
//! `default_headers` is not: it shows up in `Debug` output exactly as it
//! would on a plain `reqwest::Client`, unless its `HeaderValue` is marked
//! sensitive with `set_sensitive(true)`.
//!
//! # Timeouts
//!
//! A bare `reqwest::Client` has none by default. This one has both: 30 s
//! for the whole attempt, 10 s to connect, each configurable on the
//! builder. They bound one attempt, not the whole call.
//!
//! One trap for tests using `#[tokio::test(start_paused = true)]`: tokio's
//! auto-advancing clock fires the request timeout the moment a task blocks
//! on real socket I/O. Use real time against a local server.
//!
//! # Sharing
//!
//! `RotatingClient` is cheap to clone: clones share the connection pools,
//! the proxy cooldown state, and the rate limiter.
//!
//! Clones of a [`RotatingClientBuilder`] share a pool given through
//! [`proxy_list`](RotatingClientBuilder::proxy_list), so every client
//! built from them shares its cooldowns; see the builder for the
//! details.
//!
//! # Watching retries
//!
//! With the `tracing` feature on, every retry is logged at debug level.
//! For counters and metrics without a tracing subscriber,
//! [`on_retry`](RotatingClientBuilder::on_retry) runs a callback before
//! each retry with the failed attempt, the reason, the proxy and the
//! delay. The two work together or apart, and both hide proxy
//! credentials the same way.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod error;
mod proxy;
mod rate_limit;
mod retry;

/// Longest duration this crate's clamped knobs honour, and the ceiling
/// used when a configured duration would overflow `Instant` arithmetic.
/// Anything above it (only absurd values such as `Duration::MAX`) is
/// treated as this, so no arithmetic on a configured duration can
/// quietly turn a limit into no limit at all.
pub(crate) const MAX_DURATION: std::time::Duration =
    std::time::Duration::from_secs(60 * 60 * 24 * 365);

pub use client::{RequestBuilder, RotatingClient, RotatingClientBuilder};
pub use error::Error;
pub use proxy::ProxyList;
pub use retry::{RetryEvent, RetryReason};

/// `tracing::debug!` with the `tracing` feature on, nothing without it, so
/// call sites need no `#[cfg]` of their own. Helpers that exist only to
/// feed these call sites carry `allow(dead_code)` for feature-off builds.
macro_rules! trace_log {
    ($($arg:tt)*) => {
        #[cfg(feature = "tracing")]
        tracing::debug!($($arg)*);
    };
}
pub(crate) use trace_log;
