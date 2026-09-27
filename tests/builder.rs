//! Builder and client identity: `Clone`, shared vs. separate state, and the
//! hooks a clone keeps.

mod common;

use std::time::Duration;

use common::{quick, record_retries};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest_rotate::{ProxyList, RotatingClient, RotatingClientBuilder};
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn client_is_clone_send_and_sync() {
    fn assert_traits<T: Clone + Send + Sync + 'static>() {}
    assert_traits::<RotatingClient>();
}

#[test]
fn builder_is_clone_send_and_sync() {
    fn assert_traits<T: Clone + Send + Sync + 'static>() {}
    assert_traits::<RotatingClientBuilder>();
}

#[test]
fn builder_clones_share_a_given_proxy_list() {
    let (first, second) = ("http://proxy-a.example:8080", "http://proxy-b.example:8080");

    let pool = ProxyList::new([first, second]).unwrap();
    let builder = quick().proxy_list(pool);

    let one = builder.clone().build().unwrap();
    let other = builder.build().unwrap();

    assert!(one.proxies().mark_bad(first, Duration::from_secs(60)));
    assert!(other.proxies().in_cooldown(first));
}

#[test]
fn builder_clones_with_url_proxies_get_separate_pools() {
    let proxy = "http://proxy-a.example:8080";

    let builder = quick().proxies([proxy]);
    let one = builder.clone().build().unwrap();
    let other = builder.build().unwrap();

    assert!(one.proxies().mark_bad(proxy, Duration::from_secs(60)));
    assert!(!other.proxies().in_cooldown(proxy));
}

#[tokio::test]
async fn builder_clone_keeps_the_configure_hook() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("x-configured", "yes"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let builder = quick().configure(|builder| {
        let mut headers = HeaderMap::new();
        headers.insert("x-configured", HeaderValue::from_static("yes"));
        builder.default_headers(headers)
    });
    let clone = builder.clone();

    let response = clone.build().unwrap().get(server.uri()).await.unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn builder_clone_keeps_the_on_retry_hook() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (hook, events) = record_retries();
    let builder = quick().retries(1).on_retry(hook);
    let response = builder
        .clone()
        .build()
        .unwrap()
        .get(server.uri())
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(events.lock().unwrap().len(), 1);
}

#[test]
fn configured_header_is_hidden_from_debug_only_when_marked_sensitive() {
    let client = quick()
        .proxies(["http://127.0.0.1:9"])
        .configure(|builder| {
            let mut headers = HeaderMap::new();
            headers.insert("x-plain", HeaderValue::from_static("visible"));
            let mut secret = HeaderValue::from_static("SUPERSECRET");
            secret.set_sensitive(true);
            headers.insert("x-secret", secret);
            builder.default_headers(headers)
        })
        .build()
        .unwrap();

    let debug = format!("{client:?}");
    assert_eq!(debug.matches("\"visible\"").count(), 2, "{debug}");
    assert!(!debug.contains("SUPERSECRET"), "{debug}");
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

/// `configure` and `user_agent` run on the proxy clients too, not only the
/// direct one. A `MockServer` stands in for an HTTP proxy: for an `http://`
/// target it receives the whole request, headers included. The request is
/// built by a bare `reqwest::Client`, so it carries no default headers of
/// its own; whatever reaches the proxy came from the proxy client.
#[tokio::test]
async fn configure_and_user_agent_apply_to_the_proxy_clients() {
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("x-configured", "yes"))
        .and(header("user-agent", "reqwest-rotate-tests/2"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&proxy)
        .await;

    let client = quick()
        .proxies([proxy.uri()])
        .user_agent("reqwest-rotate-tests/2")
        .configure(|builder| {
            let mut headers = HeaderMap::new();
            headers.insert("x-configured", HeaderValue::from_static("yes"));
            builder.default_headers(headers)
        })
        .build()
        .unwrap();

    let request = reqwest::Client::new()
        .get("http://example.invalid/page")
        .build()
        .unwrap();
    let response = client.execute(request).await.unwrap();

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn user_agent_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(header("user-agent", "reqwest-rotate-tests/1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .user_agent("reqwest-rotate-tests/1")
        .build()
        .unwrap();

    let response = client.get(server.uri()).await.unwrap();
    assert_eq!(response.status(), 200);
}
