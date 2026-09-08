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
//!   Backoff is full-jitter exponential; a `Retry-After` header, when
//!   present, replaces the computed delay. If the server asks for a wait
//!   longer than `max_retry_after` (30 s by default), the response is
//!   returned right away instead of retrying early against its wishes.
//! - A `407` answered by a configured proxy, for every request including
//!   `POST`: the proxy did not forward anything, so nothing can be
//!   duplicated. The proxy is put in cooldown and the next attempt goes
//!   through another one; a `Retry-After` on that `407` is deliberately
//!   not honoured, since the wait belongs to the failed proxy, not to the
//!   server.
//! - Transport errors that prove the request never reached the server:
//!   connect failures (including connect timeouts), requests cancelled
//!   before dispatch, HTTP/2 `REFUSED_STREAM`. Retried for every request.
//! - Other transport errors (a total-request timeout, a connection closed
//!   or reset before the response arrived (the classic keep-alive race of
//!   long-running scrapers), HTTP/2 `GOAWAY`/reset): only for idempotent
//!   methods (`GET`, `HEAD`, `OPTIONS`, `PUT`, `DELETE`, `TRACE`).
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
//! it costs nothing once a single answer clears the mark. While another
//! proxy is out of cooldown, the retry goes through it right away; once no
//! other proxy is available, retries are paced by the backoff. Any other
//! status is the origin's answer, and the proxy keeps its place in the
//! rotation.
//!
//! Only proxies you configure are used: the `HTTP_PROXY`, `HTTPS_PROXY`
//! and `ALL_PROXY` environment variables are ignored. `http://` and
//! `https://` proxies work out of the box; `socks5://` and friends need
//! the `socks` cargo feature (without it they are rejected by `build()`
//! instead of silently misbehaving). A bare `host:port` is accepted and
//! treated as `http://host:port`. Proxy credentials are hidden from
//! `Debug` output, error messages, and `tracing` events.
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

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod error;
mod proxy;
mod rate_limit;
mod retry;

pub use client::{RotatingClient, RotatingClientBuilder};
pub use error::Error;
pub use proxy::ProxyList;

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
