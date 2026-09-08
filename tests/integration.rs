//! End-to-end tests against local servers: `wiremock` for ordinary HTTP
//! behaviour, and a few raw TCP listeners for things `wiremock` cannot
//! stand in for (a connection dropped mid-request, an HTTP proxy answering
//! `407`). Proxy rotation and cooldown arithmetic are unit-tested directly
//! on `ProxyList` in `src/proxy.rs`.
//!
//! These tests run on real time, not `start_paused`: the client now has a
//! request timeout, and tokio's auto-advancing paused clock would fire that
//! timer the moment a task blocks on real socket I/O.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest_rotate::{Error, ProxyList, RotatingClient, RotatingClientBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Builder with tiny backoff so retry tests don't sit around.
fn quick() -> RotatingClientBuilder {
    RotatingClient::builder().backoff(Duration::from_millis(5), Duration::from_millis(20))
}

/// A minimal HTTP server on a random port. Each connection is read once,
/// then either dropped without a reply (the first `drop_first`
/// connections) or answered with `response`. Returns the base URL and a
/// counter of accepted connections.
async fn raw_server(drop_first: usize, response: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            if seen <= drop_first {
                drop(socket);
                continue;
            }
            let _ = socket.write_all(response).await;
            let _ = socket.shutdown().await;
        }
    });
    (url, connections)
}

const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";

/// A server that answers every connection with a `503` carrying a
/// `body_len`-byte body, streamed in 64 KiB chunks until the client goes
/// away. Returns the base URL and a counter of body bytes the kernel
/// accepted for sending, i.e. roughly what the client actually read.
async fn big_error_body_server(body_len: usize) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let written = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&written);
    let chunk: Arc<[u8]> = vec![b'x'; 64 * 1024].into();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let counter = Arc::clone(&counter);
            let chunk = Arc::clone(&chunk);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {body_len}\r\n\r\n"
                );
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let mut left = body_len;
                while left > 0 {
                    let n = left.min(chunk.len());
                    if socket.write_all(&chunk[..n]).await.is_err() {
                        break;
                    }
                    counter.fetch_add(n, Ordering::SeqCst);
                    left -= n;
                }
            });
        }
    });
    (url, written)
}

#[tokio::test]
async fn get_success_returns_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(&server)
        .await;

    let client = RotatingClient::builder().build().unwrap();
    let response = client.get(format!("{}/ok", server.uri())).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn not_found_is_returned_without_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let client = quick().retries(5).build().unwrap();
    let response = client
        .get(format!("{}/missing", server.uri()))
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    // `.expect(1)` above is verified when `server` drops at end of test.
}

#[tokio::test]
async fn retries_on_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let client = quick().retries(3).build().unwrap();
    let response = client.get(format!("{}/flaky", server.uri())).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn last_response_is_returned_when_retries_run_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/always-down"))
        .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
        .expect(3) // initial try + 2 retries
        .mount(&server)
        .await;

    let client = quick().retries(2).build().unwrap();
    let response = client
        .get(format!("{}/always-down", server.uri()))
        .await
        .unwrap();

    assert_eq!(response.status(), 503);
    assert_eq!(response.text().await.unwrap(), "try later");
}

/// Body size for [`keep_alive_503_server`]: below the client's drain
/// budget, but well above what `hyper` buffers along with the headers, so
/// a body that is *not* drained really does cost the connection.
const KEEP_ALIVE_503_BODY_LEN: usize = 48 * 1024;

/// A keep-alive server that answers every request on a connection with
/// the same `503` and a [`KEEP_ALIVE_503_BODY_LEN`]-byte body. Returns the
/// base URL and a counter of accepted connections, so a test can tell
/// reuse from reconnecting.
async fn keep_alive_503_server() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    let reply: Arc<[u8]> = {
        let mut reply = format!(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {KEEP_ALIVE_503_BODY_LEN}\r\n\r\n"
        )
        .into_bytes();
        reply.resize(reply.len() + KEEP_ALIVE_503_BODY_LEN, b'b');
        reply.into()
    };
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = Arc::clone(&reply);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {
                    if socket.write_all(&reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (url, connections)
}

#[tokio::test]
async fn drained_error_body_reuses_the_connection() {
    let (url, connections) = keep_alive_503_server().await;

    let client = quick().retries(2).build().unwrap();
    let response = client.get(format!("{url}/busy")).await.unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.bytes().await.unwrap().len(),
        KEEP_ALIVE_503_BODY_LEN
    );

    // Three attempts, one connection: each drained body freed it for reuse.
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_drain_does_not_buffer_a_huge_error_body() {
    // A WAF block page or a full-HTML 503 can be megabytes; before
    // retrying, the client only needs the connection back, not the body.
    let body_len = 64 * 1024 * 1024;
    let (url, written) = big_error_body_server(body_len).await;

    let client = quick().retries(1).build().unwrap();
    let response = client.get(format!("{url}/blocked")).await.unwrap();
    assert_eq!(response.status(), 503);

    // Two attempts; the first one's body was drained before the retry.
    // Loopback socket buffers can absorb a few MB, never half the body.
    let read = written.load(Ordering::SeqCst);
    assert!(
        read < body_len / 2,
        "client read {read} bytes of the error body"
    );
}

#[tokio::test]
async fn status_408_is_retried_but_501_is_not() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(408))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/unimplemented"))
        .respond_with(ResponseTemplate::new(501))
        .expect(1)
        .mount(&server)
        .await;

    let client = quick().retries(3).build().unwrap();
    let ok = client.get(format!("{}/slow", server.uri())).await.unwrap();
    assert_eq!(ok.status(), 200);
    let nope = client
        .get(format!("{}/unimplemented", server.uri()))
        .await
        .unwrap();
    assert_eq!(nope.status(), 501);
}

#[tokio::test]
async fn retry_after_seconds_is_honoured() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Backoff itself is set tiny so only the Retry-After header can explain
    // a full-second wait; the cap is left above the header's value.
    let client = RotatingClient::builder()
        .retries(2)
        .backoff(Duration::from_millis(1), Duration::from_secs(10))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let response = client
        .get(format!("{}/limited", server.uri()))
        .await
        .unwrap();
    let elapsed = tokio::time::Instant::now() - start;

    assert_eq!(response.status(), 200);
    assert!(elapsed >= Duration::from_secs(1), "elapsed = {elapsed:?}");
}

#[tokio::test]
async fn retry_after_survives_a_tiny_backoff_cap() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Snappy backoff (5 ms base, 20 ms cap) must not silently disable
    // `Retry-After`: the server's explicit ask is a separate budget.
    let client = quick().retries(2).build().unwrap();

    let start = tokio::time::Instant::now();
    let response = client
        .get(format!("{}/limited", server.uri()))
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(response.status(), 200);
    assert!(elapsed >= Duration::from_secs(1), "elapsed = {elapsed:?}");
}

#[tokio::test]
async fn retry_after_over_the_cap_returns_the_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/go-away"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .expect(1)
        .mount(&server)
        .await;

    // The backoff cap is generous; only `max_retry_after` can explain the
    // 1 s ask being turned down.
    let client = RotatingClient::builder()
        .retries(3)
        .backoff(Duration::from_millis(1), Duration::from_secs(10))
        .max_retry_after(Duration::from_millis(500))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let response = client
        .get(format!("{}/go-away", server.uri()))
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    assert!(tokio::time::Instant::now() - start < Duration::from_secs(1));
}

#[tokio::test]
async fn retry_after_missing_falls_back_to_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/no-retry-after"))
        .respond_with(ResponseTemplate::new(429)) // no Retry-After header
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/no-retry-after"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .retries(2)
        .backoff(Duration::from_millis(50), Duration::from_millis(200))
        .build()
        .unwrap();

    let response = client
        .get(format!("{}/no-retry-after", server.uri()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn rate_limit_enforces_min_interval_per_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .rate_limit(Duration::from_millis(300))
        .build()
        .unwrap();

    // The interval is measured between request *starts*, so time from
    // before the first request, not from its completion.
    let start = tokio::time::Instant::now();
    client.get(format!("{}/a", server.uri())).await.unwrap();
    client.get(format!("{}/b", server.uri())).await.unwrap();
    let elapsed = tokio::time::Instant::now() - start;

    assert!(
        elapsed >= Duration::from_millis(300),
        "elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn clones_share_the_rate_limiter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .rate_limit(Duration::from_millis(300))
        .build()
        .unwrap();
    let other = client.clone();

    let start = tokio::time::Instant::now();
    client.get(format!("{}/a", server.uri())).await.unwrap();
    other.get(format!("{}/b", server.uri())).await.unwrap();
    let elapsed = tokio::time::Instant::now() - start;

    assert!(
        elapsed >= Duration::from_millis(300),
        "elapsed = {elapsed:?}"
    );
}

#[test]
fn client_is_clone_send_and_sync() {
    fn assert_traits<T: Clone + Send + Sync + 'static>() {}
    assert_traits::<RotatingClient>();
}

#[tokio::test]
async fn works_without_any_proxies_configured() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .proxies(Vec::<String>::new())
        .build()
        .unwrap();

    let response = client.get(server.uri()).await.unwrap();
    assert!(response.status().is_success());
}

#[tokio::test]
async fn invalid_proxy_url_is_rejected_at_build_time() {
    let err = RotatingClient::builder()
        .proxies(["not a valid proxy url"])
        .build()
        .unwrap_err();

    assert!(matches!(err, Error::InvalidProxy(_)));
}

#[cfg(not(feature = "socks"))]
#[tokio::test]
async fn socks_proxy_is_rejected_without_the_feature() {
    let err = RotatingClient::builder()
        .proxies(["socks5://127.0.0.1:1080"])
        .build()
        .unwrap_err();

    assert!(matches!(err, Error::InvalidProxy(_)), "{err}");
}

#[cfg(feature = "socks")]
#[tokio::test]
async fn socks_proxy_is_accepted_with_the_feature() {
    let client = RotatingClient::builder()
        .proxies(["socks5://127.0.0.1:1080"])
        .build()
        .unwrap();

    assert_eq!(client.proxies().len(), 1);
}

#[tokio::test]
async fn execute_sends_a_prebuilt_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exec"))
        .respond_with(ResponseTemplate::new(200).set_body_string("via execute"))
        .mount(&server)
        .await;

    let client = RotatingClient::builder().build().unwrap();
    let request = client
        .request(reqwest::Method::GET, format!("{}/exec", server.uri()))
        .build()
        .unwrap();

    let response = client.execute(request).await.unwrap();
    assert_eq!(response.text().await.unwrap(), "via execute");
}

#[tokio::test]
async fn send_retries_a_post_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submit"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/submit"))
        .respond_with(ResponseTemplate::new(200).set_body_string("accepted"))
        .mount(&server)
        .await;

    let client = quick().retries(2).build().unwrap();
    let request_builder = client
        .request(reqwest::Method::POST, format!("{}/submit", server.uri()))
        .body("payload=1");
    let response = client.send(request_builder).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "accepted");
}

#[tokio::test]
async fn post_is_not_retried_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/crashed"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let client = quick().retries(3).build().unwrap();
    let request_builder = client
        .request(reqwest::Method::POST, format!("{}/crashed", server.uri()))
        .body("payload=1");
    let response = client.send(request_builder).await.unwrap();

    // The server may have processed it; replaying could duplicate it.
    assert_eq!(response.status(), 500);
}

#[tokio::test]
async fn streaming_body_is_sent_once_without_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/stream"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let client = quick().retries(3).build().unwrap();
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter([Ok::<
        &'static [u8],
        std::io::Error,
    >(b"payload=1")]));
    let request_builder = client
        .request(reqwest::Method::POST, format!("{}/stream", server.uri()))
        .body(body);

    // A body that cannot be replayed still goes out exactly once, and the
    // response comes back instead of an error.
    let response = client.send(request_builder).await.unwrap();
    assert_eq!(response.status(), 500);
}

#[tokio::test]
async fn request_timeout_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stall"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/stall"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fast"))
        .mount(&server)
        .await;

    let client = quick()
        .retries(1)
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();

    let response = client.get(format!("{}/stall", server.uri())).await.unwrap();
    assert_eq!(response.text().await.unwrap(), "fast");
}

#[tokio::test]
async fn dropped_connection_is_retried_for_get() {
    let (url, connections) = raw_server(1, OK_RESPONSE).await;

    let client = quick().retries(2).build().unwrap();
    let response = client.get(&url).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dropped_connection_is_not_retried_for_post() {
    let (url, connections) = raw_server(1, OK_RESPONSE).await;

    let client = quick().retries(2).build().unwrap();
    let request_builder = client.request(reqwest::Method::POST, &url).body("x=1");
    let err = client.send(request_builder).await.unwrap_err();

    assert!(matches!(err, Error::Reqwest(_)), "{err}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connect_failure_marks_proxy_bad() {
    // Ports 1 and 2 are reserved TCP ports that nothing listens on, so a
    // connect attempt fails immediately instead of timing out for real.
    let bad_proxy = "http://127.0.0.1:1";
    let good_proxy = "http://127.0.0.1:2";

    let client = RotatingClient::builder()
        .proxies([bad_proxy, good_proxy])
        .retries(0)
        .proxy_cooldown(Duration::from_secs(60))
        .build()
        .unwrap();

    // Single attempt, through the first proxy in rotation; it can't
    // connect, so it should come back as a transport error.
    let result = client.get("http://example.invalid/").await;
    assert!(result.is_err());

    // The client's own proxy list should now skip the bad proxy: the next
    // pick is the good one, not a repeat of the failed one.
    assert!(client.proxies().in_cooldown(bad_proxy));
    assert!(!client.proxies().in_cooldown(good_proxy));
    assert_eq!(client.proxies().pick(), Some("http://127.0.0.1:2/"));
}

#[tokio::test]
async fn dead_pool_backs_off_instead_of_spinning() {
    // Both proxies refuse connections, so after the first two attempts the
    // whole pool is in cooldown: there is no healthy proxy to switch to,
    // and retries must be paced by the backoff like the single-proxy case.
    let client = RotatingClient::builder()
        .proxies(["http://127.0.0.1:1", "http://127.0.0.1:2"])
        .retries(10)
        .backoff(Duration::from_millis(100), Duration::from_millis(100))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let result = client.get("http://example.invalid/").await;
    let elapsed = start.elapsed();

    assert!(result.is_err());
    // Ten full-jitter delays of up to 100 ms: their sum is below 100 ms
    // with probability 1/10!; with no delay at all the call took ~1 ms.
    assert!(
        elapsed >= Duration::from_millis(100),
        "elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn zero_cooldown_single_proxy_still_backs_off() {
    // A zero cooldown means "never skip a proxy", not "the pool is never
    // dead": with a single refusing proxy, the retry must still be paced by
    // the backoff instead of spinning on the same dead proxy at full speed.
    let client = RotatingClient::builder()
        .proxies(["http://127.0.0.1:1"])
        .proxy_cooldown(Duration::ZERO)
        .retries(10)
        .backoff(Duration::from_millis(100), Duration::from_millis(100))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let result = client.get("http://example.invalid/").await;
    let elapsed = start.elapsed();

    assert!(result.is_err());
    // Same 1/10! argument as above.
    assert!(
        elapsed >= Duration::from_millis(100),
        "elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn proxy_407_ignores_retry_after() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\nretry-after: 300\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        // A large backoff: switching proxies must not wait for it.
        .backoff(Duration::from_secs(5), Duration::from_secs(5))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let response = client.get("http://example.invalid/page").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown(&auth_proxy));
}

/// Characterises existing behaviour: unlike an origin's `5xx`, a proxy's
/// `407` never reached the origin at all, so it is retried for a `POST`
/// too: the proxy failed, nothing was duplicated.
#[tokio::test]
async fn proxy_407_is_retried_for_a_post() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        .build()
        .unwrap();

    let request_builder = client
        .request(reqwest::Method::POST, "http://example.invalid/page")
        .body("x=1");
    let response = client.send(request_builder).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proxy_407_cools_down_and_rotates() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = quick()
        .proxies([auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        .build()
        .unwrap();

    // The target host never resolves: only a proxy can answer this.
    let response = client.get("http://example.invalid/page").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown(&auth_proxy));
    assert!(!client.proxies().in_cooldown(&good_proxy));
}

#[tokio::test]
async fn a_proxy_that_answers_leaves_cooldown() {
    let (first, first_seen) = raw_server(0, OK_RESPONSE).await;
    let (second, _second_seen) = raw_server(0, OK_RESPONSE).await;

    let pool = ProxyList::new([first.as_str(), second.as_str()]).unwrap();
    // 60s vs. 120s: the first proxy's cooldown ends sooner, so it is
    // unambiguously the fallback pick with both proxies cooling down.
    pool.mark_bad(&first, Duration::from_secs(60));
    pool.mark_bad(&second, Duration::from_secs(120));

    let client = quick().proxy_list(pool).retries(0).build().unwrap();

    // The target host never resolves: only a proxy can answer this.
    let response = client.get("http://example.invalid/x").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(first_seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&first));
    assert!(client.proxies().in_cooldown(&second));
}

#[tokio::test]
async fn origin_403_does_not_blame_the_proxy() {
    let (proxy, seen) = raw_server(
        0,
        b"HTTP/1.1 403 Forbidden\r\ncontent-length: 6\r\nconnection: close\r\n\r\ndenied",
    )
    .await;

    let client = quick()
        .proxies([proxy.as_str()])
        .retries(3)
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/secret").await.unwrap();

    assert_eq!(response.status(), 403);
    assert_eq!(response.text().await.unwrap(), "denied");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&proxy));
}

#[tokio::test]
async fn configure_hook_applies_to_the_underlying_clients() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("x-configured", "yes"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .configure(|builder| {
            let mut headers = HeaderMap::new();
            headers.insert("x-configured", HeaderValue::from_static("yes"));
            builder.default_headers(headers)
        })
        .build()
        .unwrap();

    let response = client.get(server.uri()).await.unwrap();
    assert_eq!(response.status(), 200);
}
