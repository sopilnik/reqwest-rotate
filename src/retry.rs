//! Backoff/jitter calculation, retryable-status and transient-error
//! classification, and `Retry-After` parsing. Pure functions, kept separate
//! from [`crate::client`] so they can be unit-tested without network I/O.

use std::time::Duration;

use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};

/// A uniform fraction in `[0, 1)` for backoff jitter. Spreading retries
/// apart is all this has to do, so it is a SplitMix64 step over one
/// atomic counter rather than a cryptographic generator. The seed comes
/// from `RandomState`, which the standard library already randomises per
/// process, so two processes started together do not retry in lockstep.
fn jitter_fraction() -> f64 {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    static STATE: OnceLock<AtomicU64> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let seed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        AtomicU64::new(seed)
    });

    let mut z = state
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 53 bits of mantissa is the whole precision an f64 in [0, 1) has.
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// Computes the delay before a retry attempt using "full jitter" exponential
/// backoff: a delay sampled uniformly from `[0, min(max, base * 2^attempt)]`.
///
/// `attempt` is zero-based (0 = delay before the *first* retry, i.e. after
/// the initial request failed).
pub(crate) fn backoff_delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    // Cap the shift so it can't overflow for a large attempt count; by the
    // time the exponent is this big the delay is clamped to `max` anyway,
    // so the exact multiplier no longer matters.
    let multiplier = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let capped = base.saturating_mul(multiplier).min(max);
    capped.mul_f64(jitter_fraction())
}

/// Returns `true` for a status worth another attempt.
///
/// 408, 429 and 503 say the request was not processed, so they are retried
/// for every method. Other 5xx (except 501 and 505, which never get better
/// by asking again) are retried only for `idempotent` requests: a `POST`
/// the server processed and then failed to answer would be duplicated.
pub(crate) fn is_retryable_status(status: StatusCode, idempotent: bool) -> bool {
    match status.as_u16() {
        408 | 429 | 503 => true,
        501 | 505 => false,
        code => idempotent && (500..=599).contains(&code),
    }
}

/// `407 Proxy Authentication Required` is the only status that can come
/// from the proxy rather than the origin. Everything else belongs to the
/// caller.
pub(crate) fn is_proxy_failure_status(status: StatusCode) -> bool {
    status == StatusCode::PROXY_AUTHENTICATION_REQUIRED
}

/// Methods that can be repeated safely when it is unknown whether the
/// server processed the first try.
pub(crate) fn is_idempotent(method: &Method) -> bool {
    matches!(
        method.as_str(),
        "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE" | "TRACE"
    )
}

/// Every error in the chain below a `reqwest::Error`.
fn sources(err: &reqwest::Error) -> impl Iterator<Item = &(dyn std::error::Error + 'static)> {
    std::iter::successors(std::error::Error::source(err), |inner| inner.source())
}

/// Returns `true` if the request failed at the connection level rather
/// than being answered: connect failure, timeout, a connection dropped or
/// reset before the response arrived, or an HTTP/2 stream/connection
/// error. Builder, redirect-policy, decode and body errors are not
/// transport errors.
///
/// This is what decides whether a *proxy* gets blamed: every one of these,
/// seen through a proxy, means the proxy did not deliver.
pub(crate) fn is_transport_error(err: &reqwest::Error) -> bool {
    if err.is_connect() || err.is_timeout() {
        return true;
    }
    if !err.is_request() {
        return false;
    }
    sources(err).any(|inner| {
        inner
            .downcast_ref::<hyper::Error>()
            .is_some_and(|e| !e.is_user() && !e.is_parse())
            || inner.downcast_ref::<std::io::Error>().is_some()
            || inner.downcast_ref::<h2::Error>().is_some()
    })
}

/// Returns `true` for a transport error that proves the server never
/// received the request, so replaying it is safe even for a `POST`: a
/// connect failure (including a connect timeout), a request `hyper`
/// cancelled before dispatching it, or an HTTP/2 `REFUSED_STREAM`.
pub(crate) fn is_never_sent_error(err: &reqwest::Error) -> bool {
    if err.is_connect() {
        return true;
    }
    sources(err).any(|inner| {
        inner
            .downcast_ref::<hyper::Error>()
            .is_some_and(hyper::Error::is_canceled)
            || inner
                .downcast_ref::<h2::Error>()
                .is_some_and(|e| e.reason() == Some(h2::Reason::REFUSED_STREAM))
    })
}

/// Decides whether a transport error is worth another attempt: always when
/// the request provably never reached the server, otherwise only for
/// `idempotent` requests, where a duplicate is harmless. A total request
/// timeout on a `POST` is therefore *not* retried: the server may be in
/// the middle of processing it.
pub(crate) fn should_retry_error(err: &reqwest::Error, idempotent: bool) -> bool {
    is_never_sent_error(err) || (idempotent && is_transport_error(err))
}

/// Short label for a transport failure, for log lines that must not
/// carry the error's own text (it embeds the request URL).
#[cfg_attr(not(feature = "tracing"), allow(dead_code))]
pub(crate) fn error_kind(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if is_never_sent_error(err) {
        "never sent"
    } else if is_transport_error(err) {
        "transport"
    } else {
        "other"
    }
}

/// Parses a `Retry-After` header value: either delta-seconds (`"120"`) or an
/// HTTP-date (`"Wed, 21 Oct 2015 07:28:00 GMT"`). Returns `None` if the
/// header is absent, unparseable, or names a time already in the past.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();

    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let target = httpdate::parse_http_date(value).ok()?;
    target.duration_since(std::time::SystemTime::now()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    #[test]
    fn backoff_never_exceeds_max() {
        let max = Duration::from_secs(10);
        for attempt in 0..40 {
            let delay = backoff_delay(attempt, Duration::from_millis(100), max);
            assert!(delay <= max, "attempt {attempt} produced {delay:?}");
        }
    }

    #[test]
    fn backoff_jitter_reaches_the_cap() {
        // Full jitter samples uniformly from [0, cap], so across enough
        // draws at least one should land close to the cap itself.
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(10);
        let saw_a_large_sample = (0..1000)
            .map(|_| backoff_delay(0, base, max))
            .any(|d| d > base.mul_f64(0.9));
        assert!(saw_a_large_sample);
    }

    #[test]
    fn jitter_is_uniform_over_the_unit_interval() {
        let samples: Vec<f64> = (0..100_000).map(|_| jitter_fraction()).collect();
        for &sample in &samples {
            assert!((0.0..1.0).contains(&sample), "{sample}");
        }
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        assert!(min < 0.01, "min {min}");
        assert!(max > 0.99, "max {max}");
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!((0.48..=0.52).contains(&mean), "mean {mean}");
    }

    #[test]
    fn idempotent_retries_408_429_and_5xx() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            // Cloudflare-style origin errors are 5xx too.
            StatusCode::from_u16(522).unwrap(),
        ] {
            assert!(is_retryable_status(status, true), "{status}");
        }
        for status in [
            StatusCode::NOT_IMPLEMENTED,
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
            StatusCode::NOT_FOUND,
            StatusCode::FORBIDDEN,
            StatusCode::OK,
        ] {
            assert!(!is_retryable_status(status, true), "{status}");
        }
    }

    #[test]
    fn non_idempotent_retries_only_unprocessed_statuses() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(is_retryable_status(status, false), "{status}");
        }
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::NOT_IMPLEMENTED,
        ] {
            assert!(!is_retryable_status(status, false), "{status}");
        }
    }

    #[test]
    fn proxy_failure_status_is_407_only() {
        assert!(is_proxy_failure_status(
            StatusCode::PROXY_AUTHENTICATION_REQUIRED
        ));
        assert!(!is_proxy_failure_status(StatusCode::FORBIDDEN));
        assert!(!is_proxy_failure_status(StatusCode::NOT_FOUND));
        assert!(!is_proxy_failure_status(StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn idempotent_methods() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::OPTIONS,
            Method::PUT,
            Method::DELETE,
            Method::TRACE,
        ] {
            assert!(is_idempotent(&method), "{method}");
        }
        assert!(!is_idempotent(&Method::POST));
        assert!(!is_idempotent(&Method::PATCH));
        assert!(!is_idempotent(&Method::CONNECT));
    }

    #[test]
    fn retry_after_parses_delta_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("120"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(120)));
    }

    #[test]
    fn retry_after_parses_http_date() {
        let future = std::time::SystemTime::now() + Duration::from_secs(60);
        let formatted = httpdate::fmt_http_date(future);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(&formatted).unwrap());
        let parsed = retry_after(&headers).unwrap();
        // httpdate has one-second resolution; allow a little tolerance.
        assert!((58..=60).contains(&parsed.as_secs()), "{parsed:?}");
    }

    #[test]
    fn retry_after_past_date_is_none() {
        let past = std::time::SystemTime::now() - Duration::from_secs(60);
        let formatted = httpdate::fmt_http_date(past);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(&formatted).unwrap());
        assert_eq!(retry_after(&headers), None);
    }

    #[test]
    fn retry_after_missing_header_is_none() {
        let headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
    }

    #[tokio::test]
    async fn error_kind_labels_a_connect_failure() {
        let err = reqwest::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .unwrap_err();
        assert_eq!(error_kind(&err), "connect");

        let builder_err = reqwest::Proxy::all("http://[").unwrap_err();
        assert_eq!(error_kind(&builder_err), "other");
    }

    #[tokio::test]
    async fn error_kind_labels_a_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept the connection but hold it open without ever writing a
        // response, so the client's request timeout fires instead of a
        // connect error.
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(30));
                drop(stream);
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(1))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(error_kind(&err), "timeout");
    }

    #[tokio::test]
    async fn error_kind_labels_a_truncated_body_as_other() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Answer with a Content-Length longer than what actually follows,
        // then drop the connection: the client reads the headers fine but
        // the body comes up short.
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                // Drain the request first: closing a socket with unread
                // data queued sends a reset instead of a clean FIN, which
                // would surface as a connect-level error instead of the
                // truncated-body error this test wants.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n");
                drop(stream);
            }
        });

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let err = response.bytes().await.unwrap_err();
        // A short body surfaces as `kind: Decode`, not `kind: Request`, so
        // `is_transport_error`'s `!err.is_request()` guard skips it here.
        assert_eq!(error_kind(&err), "other");
    }

    #[tokio::test]
    async fn error_kind_labels_a_dropped_connection_as_transport() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Drop the connection before writing any response bytes: no header
        // line arrives, so this is `kind: Request`, squarely the case
        // `is_transport_error` exists for.
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Read;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                drop(stream);
            }
        });

        let err = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(error_kind(&err), "transport");
    }

    // No test for error_kind's "never sent" branch: a canceled hyper request
    // or an h2 REFUSED_STREAM needs a real HTTP/2 server, not a TcpListener.

    #[test]
    fn retry_after_garbage_value_is_none() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("not-a-date"));
        assert_eq!(retry_after(&headers), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("-5"));
        assert_eq!(retry_after(&headers), None);
    }
}
