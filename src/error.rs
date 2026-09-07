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
/// [`source`](std::error::Error::source) and the variant's own message
/// does not repeat it.
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
    /// does not support (SOCKS schemes need the `socks` feature).
    #[error("invalid proxy: {0}")]
    InvalidProxy(String),

    /// The underlying `reqwest::Client` could not be built: an invalid TLS
    /// or proxy configuration, usually.
    #[error("failed to build the underlying client")]
    Build(#[source] reqwest::Error),
}

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
}
