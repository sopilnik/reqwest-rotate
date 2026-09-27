//! Which statuses and transport errors are retried, and for which methods.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{
    KEEP_ALIVE_503_BODY_LEN, OK_RESPONSE, big_error_body_server, keep_alive_503_server, quick,
    raw_server, stalled_503_body_server,
};
use reqwest_rotate::{Error, RotatingClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

#[tokio::test]
async fn zero_retries_returns_retryable_response_after_one_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/no-retries"))
        .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
        .expect(1)
        .mount(&server)
        .await;

    let client = quick().retries(0).build().unwrap();
    let response = client
        .get(format!("{}/no-retries", server.uri()))
        .await
        .unwrap();

    assert_eq!(response.status(), 503);
    assert_eq!(response.text().await.unwrap(), "try later");
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
async fn a_stalled_error_body_does_not_hold_up_the_retry() {
    let (url, second) = stalled_503_body_server().await;
    let client = quick()
        .retries(1)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let start = std::time::Instant::now();
    let response = client.get(format!("{url}/slow")).await.unwrap();
    assert_eq!(response.status(), 200);
    let waited = second.lock().unwrap().unwrap() - start;
    // The drain gives up after 250 ms; before, it waited out the 30 s
    // per-attempt timeout.
    assert!(
        waited < Duration::from_secs(10),
        "retry started after {waited:?}"
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
    let response = request_builder.send().await.unwrap();

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
    let response = request_builder.send().await.unwrap();

    // The server may have processed it; replaying could duplicate it.
    assert_eq!(response.status(), 500);
}

#[tokio::test]
async fn streaming_body_is_sent_once_without_retries() {
    let server = MockServer::start().await;
    // 503 is retryable for every method, so this exercises the line that
    // stops a second attempt from running when the body cannot be
    // replayed: a 500 would return on the first attempt anyway, POST not
    // being idempotent, and would not catch a regression there.
    Mock::given(method("POST"))
        .and(path("/stream"))
        .respond_with(ResponseTemplate::new(503))
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
    let response = request_builder.send().await.unwrap();
    assert_eq!(response.status(), 503);
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

/// A per-attempt timeout on a `POST` is not retried: the server may be in
/// the middle of processing it. `.expect(1)` fails the test on a replay.
#[tokio::test]
async fn post_timeout_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/stall"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .expect(1)
        .mount(&server)
        .await;

    let client = quick()
        .retries(2)
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let err = client
        .request(reqwest::Method::POST, format!("{}/stall", server.uri()))
        .body("x=1")
        .send()
        .await
        .unwrap_err();

    assert!(
        matches!(&err, Error::Reqwest(e) if e.is_timeout()),
        "{err:?}"
    );
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
    let err = request_builder.send().await.unwrap_err();

    assert!(matches!(err, Error::Reqwest(_)), "{err}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}
