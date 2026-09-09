//! The error type returned by this crate.

use thiserror::Error;

/// Errors produced by [`RotatingClient`](crate::RotatingClient) and its
/// builder.
///
/// HTTP status codes are never errors: after the last attempt the response
/// is handed back as-is, whatever its status, exactly like `reqwest` does.
/// Use [`Response::error_for_status`](reqwest::Response::error_for_status)
/// if you want a non-2xx status to become an error.
///
/// Where a variant wraps a `reqwest::Error`, that error is this one's
/// [`source`](std::error::Error::source); [`Reqwest`](Error::Reqwest) and
/// [`Build`](Error::Build) do not repeat it in their own message.
/// [`InvalidProxy`](Error::InvalidProxy) is the exception: its message
/// includes the reason for readability even though the same text is also
/// reachable through `source`.
///
/// Marked `#[non_exhaustive]`: new variants may be added in a minor release
/// without that counting as a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A request failed at the `reqwest` layer (network error, timeout,
    /// invalid URL, ...). Returned directly for failures that are not
    /// retried, and as the last attempt's failure once retries are used up
    /// on a transient transport error.
    #[error("request failed")]
    Reqwest(#[from] reqwest::Error),

    /// A proxy URL is blank, cannot be parsed, or uses a scheme this build
    /// does not support (SOCKS schemes need the `socks` feature); or a
    /// syntactically valid proxy was rejected when the underlying
    /// `reqwest::Client` was built.
    ///
    /// [`source`](std::error::Error::source) carries the reason: either a
    /// plain-text explanation with no cause of its own, or the
    /// `reqwest::Error` from the failed client build.
    #[error("invalid proxy: {proxy}: {source}")]
    InvalidProxy {
        /// The proxy URL as far as it could be read, or the caller's own
        /// spelling if it could not be parsed at all. Credentials are
        /// always redacted.
        proxy: String,
        /// Why the proxy was rejected.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// The underlying `reqwest::Client` could not be built: an invalid TLS
    /// or proxy configuration, usually.
    #[error("failed to build the underlying client")]
    Build(#[source] reqwest::Error),
}

impl Error {
    /// Builds [`Error::InvalidProxy`] from a redacted proxy string and a
    /// plain-text reason, for a validation failure that has no underlying
    /// error of its own.
    pub(crate) fn invalid_proxy(proxy: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::InvalidProxy {
            proxy: proxy.into(),
            source: Box::new(ProxyReason(reason.into())),
        }
    }
}

/// A plain-text reason for rejecting a proxy URL, with no underlying cause
/// of its own.
#[derive(Debug)]
struct ProxyReason(String);

impl std::fmt::Display for ProxyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProxyReason {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_keeps_its_source() {
        let inner = reqwest::Proxy::all("http://[").unwrap_err();
        let err = Error::Build(inner);
        assert!(std::error::Error::source(&err).is_some());
        assert_eq!(err.to_string(), "failed to build the underlying client");
    }

    #[test]
    fn reqwest_display_is_bare() {
        let inner = reqwest::Proxy::all("http://[").unwrap_err();
        let err = Error::from(inner);
        assert!(std::error::Error::source(&err).is_some());
        assert_eq!(err.to_string(), "request failed");
    }

    #[test]
    fn invalid_proxy_keeps_its_source() {
        let err = Error::invalid_proxy("http://proxy.example", "not a valid proxy URL");
        assert!(std::error::Error::source(&err).is_some());
        assert_eq!(
            err.to_string(),
            "invalid proxy: http://proxy.example: not a valid proxy URL"
        );
    }
}
