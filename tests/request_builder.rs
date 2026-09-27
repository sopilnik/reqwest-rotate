//! `RequestBuilder`'s pass-through setters: each one reaches the server, and
//! `send()` still goes through the same retry path as `get()`.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{OK_RESPONSE, quick, raw_server};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest_rotate::{Error, RotatingClient};
#[cfg(feature = "json")]
use wiremock::matchers::body_json_string;
#[cfg(feature = "form")]
use wiremock::matchers::body_string;
#[cfg(feature = "query")]
use wiremock::matchers::query_param;
use wiremock::matchers::{basic_auth, bearer_token, header, method, path};
#[cfg(feature = "multipart")]
use wiremock::matchers::{body_string_contains, header_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn request_builder_headers_reach_the_server_and_the_send_still_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/retry-builder"))
        .and(header("x-one", "1"))
        .and(header("x-two", "2"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/retry-builder"))
        .and(header("x-one", "1"))
        .and(header("x-two", "2"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut extra = HeaderMap::new();
    extra.insert("x-two", HeaderValue::from_static("2"));

    // A single response by itself would not tell header() and headers()
    // apart from a plain reqwest::RequestBuilder; the retry on top proves
    // this went through send_with_retry, not a bypassed .send().
    let client = quick().retries(1).build().unwrap();
    let response = client
        .request(
            reqwest::Method::GET,
            format!("{}/retry-builder", server.uri()),
        )
        .header("x-one", "1")
        .headers(extra)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn request_builder_basic_and_bearer_auth_reach_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/basic"))
        .and(basic_auth("alice", "s3cret"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/bearer"))
        .and(bearer_token("tok123"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = RotatingClient::builder().build().unwrap();
    let basic = client
        .request(reqwest::Method::GET, format!("{}/basic", server.uri()))
        .basic_auth("alice", Some("s3cret"))
        .send()
        .await
        .unwrap();
    let bearer = client
        .request(reqwest::Method::GET, format!("{}/bearer", server.uri()))
        .bearer_auth("tok123")
        .send()
        .await
        .unwrap();

    assert_eq!(basic.status(), 200);
    assert_eq!(bearer.status(), 200);
}

#[cfg(feature = "query")]
#[tokio::test]
async fn request_builder_query_reaches_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rust"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = RotatingClient::builder().build().unwrap();
    let search = client
        .request(reqwest::Method::GET, format!("{}/search", server.uri()))
        .query(&[("q", "rust")])
        .send()
        .await
        .unwrap();

    assert_eq!(search.status(), 200);
}

#[cfg(feature = "form")]
#[tokio::test]
async fn request_builder_form_reaches_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submit"))
        .and(body_string("name=alice"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = RotatingClient::builder().build().unwrap();
    let submit = client
        .request(reqwest::Method::POST, format!("{}/submit", server.uri()))
        .form(&[("name", "alice")])
        .send()
        .await
        .unwrap();

    assert_eq!(submit.status(), 200);
}

#[cfg(feature = "json")]
#[tokio::test]
async fn request_builder_json_reaches_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/json"))
        .and(body_json_string(r#"{"q":"rust"}"#))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut body = std::collections::BTreeMap::new();
    body.insert("q", "rust");

    let client = RotatingClient::builder().build().unwrap();
    let response = client
        .request(reqwest::Method::POST, format!("{}/json", server.uri()))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

#[cfg(feature = "multipart")]
#[tokio::test]
async fn request_builder_multipart_reaches_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/multipart"))
        .and(header_regex("content-type", "^multipart/form-data"))
        .and(body_string_contains("hello from a field"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let form = reqwest::multipart::Form::new().text("greeting", "hello from a field");

    let client = RotatingClient::builder().build().unwrap();
    let response = client
        .request(reqwest::Method::POST, format!("{}/multipart", server.uri()))
        .multipart(form)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn request_builder_timeout_overrides_the_clients_default() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;

    let client = quick().retries(0).build().unwrap();
    let start = tokio::time::Instant::now();
    let err = client
        .request(reqwest::Method::GET, format!("{}/slow", server.uri()))
        .timeout(Duration::from_millis(200))
        .send()
        .await
        .unwrap_err();
    let elapsed = start.elapsed();

    assert!(matches!(err, Error::Reqwest(_)), "{err}");
    assert!(elapsed < Duration::from_secs(1), "elapsed = {elapsed:?}");
}

#[test]
fn request_builder_version_sets_the_built_requests_version() {
    let client = RotatingClient::builder().build().unwrap();
    let request = client
        .request(reqwest::Method::GET, "http://example.invalid/")
        .version(reqwest::Version::HTTP_10)
        .build()
        .unwrap();

    assert_eq!(request.version(), reqwest::Version::HTTP_10);
}

#[tokio::test]
async fn request_builder_into_inner_escapes_to_the_plain_builder() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/inner"))
        .respond_with(ResponseTemplate::new(200).set_body_string("plain"))
        .expect(1)
        .mount(&server)
        .await;
    let (proxy, seen) = raw_server(0, OK_RESPONSE).await;

    let client = RotatingClient::builder()
        .proxies([proxy.as_str()])
        .build()
        .unwrap();
    let response = client
        .request(reqwest::Method::GET, format!("{}/inner", server.uri()))
        .into_inner()
        .send()
        .await
        .unwrap();

    assert_eq!(response.text().await.unwrap(), "plain");
    assert_eq!(seen.load(Ordering::SeqCst), 0);
}
