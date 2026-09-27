mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{OK_RESPONSE, quick, raw_server, record_retries};
use reqwest::StatusCode;
use reqwest_rotate::{Error, RetryEvent, RetryReason, RotatingClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn on_retry_reports_each_retried_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (hook, events) = record_retries();
    let client = quick().retries(3).on_retry(hook).build().unwrap();
    let response = client.get(format!("{}/flaky", server.uri())).await.unwrap();
    assert_eq!(response.status(), 200);

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].attempt, 0);
    assert_eq!(events[1].attempt, 1);
    for event in events.iter() {
        assert_eq!(
            event.reason,
            RetryReason::Status(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(event.proxy, None);
        assert_eq!(event.host.as_deref(), Some("127.0.0.1"));
        assert!(
            event.delay <= Duration::from_millis(20),
            "{:?}",
            event.delay
        );
    }
}

#[tokio::test]
async fn on_retry_is_silent_without_a_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/down"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let (hook, events) = record_retries();
    let client = quick().retries(0).on_retry(hook).build().unwrap();

    let response = client.get(format!("{}/ok", server.uri())).await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(events.lock().unwrap().is_empty());

    let response = client.get(format!("{}/down", server.uri())).await.unwrap();
    assert_eq!(response.status(), 503);
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn on_retry_reports_the_delay_it_waits() {
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

    // Backoff is at most 1 ms here, so the header alone sets the delay.
    let (hook, events) = record_retries();
    let client = RotatingClient::builder()
        .retries(1)
        .backoff(Duration::from_millis(1), Duration::from_secs(10))
        .on_retry(hook)
        .build()
        .unwrap();
    let response = client
        .get(format!("{}/limited", server.uri()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].reason,
        RetryReason::Status(StatusCode::TOO_MANY_REQUESTS)
    );
    assert_eq!(events[0].delay, Duration::from_secs(1));
}

#[tokio::test]
async fn on_retry_redacts_the_proxy() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let credentialed_auth_proxy = auth_proxy.replacen("http://", "http://user:secret@", 1);
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let (hook, events) = record_retries();
    let client = quick()
        .proxies([credentialed_auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        .on_retry(hook)
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/page").await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].reason,
        RetryReason::ProxyStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
    );
    let proxy = events[0].proxy.as_deref().expect("proxy is set");
    assert!(proxy.contains("***@"), "{proxy}");
    assert!(!proxy.contains("secret"), "{proxy}");
    assert_eq!(events[0].delay, Duration::ZERO);
}

#[tokio::test]
async fn on_retry_names_the_transport_failure() {
    let (url, connections) = raw_server(1, OK_RESPONSE).await;

    let (hook, events) = record_retries();
    let client = quick().retries(2).on_retry(hook).build().unwrap();
    let response = client.get(&url).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(connections.load(Ordering::SeqCst), 2);

    {
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, RetryReason::Transport);
        assert_eq!(events[0].proxy, None);
        assert_eq!(events[0].host.as_deref(), Some("127.0.0.1"));
    }

    let (good_proxy, good_seen) = raw_server(0, OK_RESPONSE).await;
    let (hook, events) = record_retries();
    let client = RotatingClient::builder()
        .proxies(["http://user:secret@127.0.0.1:1", good_proxy.as_str()])
        .retries(1)
        .on_retry(hook)
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/").await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, RetryReason::Connect);
    assert_eq!(events[0].proxy.as_deref(), Some("http://***@127.0.0.1:1/"));
    assert_eq!(events[0].host.as_deref(), Some("example.invalid"));
}

#[tokio::test]
async fn on_retry_is_silent_after_the_last_transport_failure() {
    // Two dropped connections with one retry: the first failure is
    // retried and reported, the second is returned and is not.
    let (url, connections) = raw_server(2, OK_RESPONSE).await;
    let (hook, events) = record_retries();
    let client = quick().retries(1).on_retry(hook).build().unwrap();
    let err = client.get(&url).await.unwrap_err();
    assert!(matches!(err, Error::Reqwest(_)), "{err}");
    assert_eq!(connections.load(Ordering::SeqCst), 2);
    assert_eq!(events.lock().unwrap().len(), 1);

    // A dropped POST is not retried at all, so nothing is reported.
    let (url, connections) = raw_server(1, OK_RESPONSE).await;
    let (hook, events) = record_retries();
    let client = quick().retries(2).on_retry(hook).build().unwrap();
    let request_builder = client.request(reqwest::Method::POST, &url).body("x=1");
    let err = request_builder.send().await.unwrap_err();
    assert!(matches!(err, Error::Reqwest(_)), "{err}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn retry_event_is_send_sync_clone_debug() {
    fn assert_traits<T: Clone + std::fmt::Debug + Send + Sync + 'static>() {}
    assert_traits::<RetryEvent>();
}
