mod common;

use std::time::Duration;

use common::quick;
use reqwest_rotate::RotatingClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let elapsed = start.elapsed();

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
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn retry_after_exactly_at_the_cap_is_honoured() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // A zero ask against a zero cap: equal, so honoured, with no real wait.
    let client = quick()
        .retries(1)
        .max_retry_after(Duration::ZERO)
        .build()
        .unwrap();

    let response = client
        .get(format!("{}/limited", server.uri()))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
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

/// End to end, a `Retry-After: 0` is retried and answered. That the wait is
/// the backoff rather than zero cannot be timed under full jitter, so
/// `retry_after_zero_waits_the_backoff` in `src/retry.rs` pins that part.
#[tokio::test]
async fn retry_after_zero_is_still_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/zero"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/zero"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = quick().retries(2).build().unwrap();
    let response = client.get(format!("{}/zero", server.uri())).await.unwrap();

    assert_eq!(response.status(), 200);
}
