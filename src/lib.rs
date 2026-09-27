#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
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

#[cfg(test)]
mod tests {
    #[test]
    fn max_duration_is_one_year_in_seconds() {
        assert_eq!(
            crate::MAX_DURATION,
            std::time::Duration::from_secs(31_536_000)
        );
    }
}
