//! HTTP/2-specific retry behaviour against a local cleartext `h2` server:
//! which stream resets get replayed, and how many streams the server
//! actually saw. `tests/integration.rs` covers HTTP/1 and shares none of
//! this file's helpers, since here a connection hands out a script of
//! per-stream actions instead of one fixed byte response.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use h2::Reason;
use hyper::body::Bytes;
use reqwest::Version;
use reqwest_rotate::{Error, RotatingClient, RotatingClientBuilder};

/// Builder with tiny backoff, configured to speak HTTP/2 straight away: the
/// test server below expects the same prior knowledge, with no ALPN or
/// `Upgrade:` dance. `reqwest` itself now retries a `REFUSED_STREAM` or a
/// graceful `GOAWAY` transparently, for any method, before this crate ever
/// sees an error; `retry(retry::never())` turns that off so these tests
/// exercise `send_with_retry`'s own classification instead of reqwest's.
fn quick() -> RotatingClientBuilder {
    RotatingClient::builder()
        .backoff(Duration::from_millis(5), Duration::from_millis(20))
        .configure(|b| b.http2_prior_knowledge().retry(reqwest::retry::never()))
}

/// What the server does with one stream, in the order [`h2_server`] is
/// given them.
enum StreamAction {
    Reset(Reason),
    Ok(&'static str),
}

/// A local HTTP/2 server on a random port, accepting a single TCP
/// connection: every attempt in a test is expected to reuse it, so a
/// stream index and a connection index are the same thing here. Each
/// accepted stream is handled with the next [`StreamAction`] from
/// `script`; a stream past the end of the script is reset with
/// `INTERNAL_ERROR`. Returns the base URL and a counter of streams the
/// server accepted.
async fn h2_server(script: Vec<StreamAction>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut conn) = h2::server::handshake(socket).await else {
            return;
        };
        let mut script = script.into_iter();
        while let Some(Ok((_request, mut respond))) = conn.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            match script.next() {
                Some(StreamAction::Reset(reason)) => respond.send_reset(reason),
                Some(StreamAction::Ok(body)) => {
                    let response = http::Response::builder().status(200).body(()).unwrap();
                    if let Ok(mut send) = respond.send_response(response, false) {
                        let _ = send.send_data(Bytes::from_static(body.as_bytes()), true);
                    }
                }
                None => respond.send_reset(Reason::INTERNAL_ERROR),
            }
        }
    });
    (url, seen)
}

#[tokio::test]
async fn post_is_retried_after_a_refused_stream() {
    let (url, seen) = h2_server(vec![
        StreamAction::Reset(Reason::REFUSED_STREAM),
        StreamAction::Ok("ok"),
    ])
    .await;

    let client = quick().retries(1).build().unwrap();
    let response = client
        .request(reqwest::Method::POST, &url)
        .body("payload=1")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), Version::HTTP_2);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn post_is_not_retried_after_a_different_h2_reset() {
    let (url, seen) = h2_server(vec![StreamAction::Reset(Reason::INTERNAL_ERROR)]).await;

    let client = quick().retries(2).build().unwrap();
    let err = client
        .request(reqwest::Method::POST, &url)
        .body("payload=1")
        .send()
        .await
        .unwrap_err();

    // The server never sent a response for this reason, so it might have
    // started processing the POST; replaying it could duplicate it.
    let Error::Reqwest(inner) = err else {
        panic!("{err}")
    };
    assert!(!inner.is_timeout(), "{inner}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn get_is_retried_after_a_different_h2_reset() {
    let (url, seen) = h2_server(vec![
        StreamAction::Reset(Reason::INTERNAL_ERROR),
        StreamAction::Ok("ok"),
    ])
    .await;

    let client = quick().retries(1).build().unwrap();
    let response = client.get(&url).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refused_stream_exhausts_retries() {
    let (url, seen) = h2_server(vec![
        StreamAction::Reset(Reason::REFUSED_STREAM),
        StreamAction::Reset(Reason::REFUSED_STREAM),
        StreamAction::Reset(Reason::REFUSED_STREAM),
    ])
    .await;

    let client = quick().retries(2).build().unwrap();
    let err = client
        .request(reqwest::Method::POST, &url)
        .body("payload=1")
        .send()
        .await
        .unwrap_err();

    let Error::Reqwest(inner) = err else {
        panic!("{err}")
    };
    assert!(!inner.is_timeout(), "{inner}");
    assert_eq!(seen.load(Ordering::SeqCst), 3);
}

/// A server whose first connection sends a graceful `GOAWAY(NO_ERROR,
/// last_stream_id=0)` before reading a single frame off it, leaving the
/// request's own stream unprocessed; every later connection answers `200`.
/// Returns the base URL and a counter of TCP connections accepted, so a
/// test can tell whether a retry opened a second one.
async fn goaway_then_ok_server() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let Ok(mut conn) = h2::server::handshake(socket).await else {
                continue;
            };
            if n == 1 {
                conn.abrupt_shutdown(Reason::NO_ERROR);
                while conn.accept().await.is_some() {}
                continue;
            }
            while let Some(Ok((_request, mut respond))) = conn.accept().await {
                let response = http::Response::builder().status(200).body(()).unwrap();
                if let Ok(mut send) = respond.send_response(response, false) {
                    let _ = send.send_data(Bytes::from_static(b"ok"), true);
                }
            }
        }
    });
    (url, connections)
}

/// Matches the crate doc's claim that a `GOAWAY` is "retried only for
/// idempotent methods": with the request's own stream left unprocessed by
/// the first connection's `GOAWAY(NO_ERROR)`, a `GET` is worth retrying on
/// a fresh connection.
#[tokio::test]
async fn get_is_retried_after_an_unprocessed_goaway() {
    let (url, connections) = goaway_then_ok_server().await;

    let client = quick().retries(1).build().unwrap();
    let response = client.get(&url).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

/// Same `GOAWAY(NO_ERROR)`, but for a `POST`: not `REFUSED_STREAM`, so it
/// is not "never sent" either, and this crate only retries a plain
/// transport error for idempotent methods. No second connection is opened.
#[tokio::test]
async fn post_is_not_retried_after_an_unprocessed_goaway() {
    let (url, connections) = goaway_then_ok_server().await;

    let client = quick().retries(1).build().unwrap();
    let err = client
        .request(reqwest::Method::POST, &url)
        .body("payload=1")
        .send()
        .await
        .unwrap_err();

    let Error::Reqwest(inner) = err else {
        panic!("{err}")
    };
    assert!(!inner.is_timeout(), "{inner}");
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}
