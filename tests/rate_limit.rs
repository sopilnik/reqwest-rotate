//! Per-host pacing, including across clones and across retry attempts.

mod common;

use std::time::Duration;

use common::quick;
use reqwest_rotate::RotatingClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(300),
        "elapsed = {elapsed:?}"
    );
}

/// Two servers on 127.0.0.1 with different ports are one host to the
/// limiter, so the second request waits out the first one's interval.
#[tokio::test]
async fn rate_limit_key_ignores_the_port() {
    let one = MockServer::start().await;
    let two = MockServer::start().await;
    for server in [&one, &two] {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    let client = RotatingClient::builder()
        .rate_limit(Duration::from_millis(300))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    client.get(one.uri()).await.unwrap();
    client.get(two.uri()).await.unwrap();
    let elapsed = start.elapsed();

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
    let elapsed = start.elapsed();

    assert!(
        elapsed >= Duration::from_millis(300),
        "elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn each_retry_attempt_takes_its_own_rate_limit_slot() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/throttled"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/throttled"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Backoff is tiny, so only two rate-limit slots (one per attempt) can
    // explain a wait this long: a single slot shared across the retry
    // would let the second attempt through immediately.
    let client = quick()
        .retries(1)
        .rate_limit(Duration::from_millis(300))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let response = client
        .get(format!("{}/throttled", server.uri()))
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(response.status(), 200);
    assert!(
        elapsed >= Duration::from_millis(300),
        "elapsed = {elapsed:?}"
    );
}
