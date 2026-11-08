//! HTTP/2-specific retry behaviour against a local cleartext `h2` server:
//! which stream resets get replayed, and how many streams the server
//! actually saw. The server is local to this file, since a connection here
//! hands out a script of per-stream actions instead of one fixed byte
//! response; only the shared test helpers come from `common`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use h2::Reason;
use hyper::body::Bytes;
use reqwest::Version;
use reqwest_rotate::{Error, RetryReason, RotatingClient, RotatingClientBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Builder with tiny backoff, configured to speak HTTP/2 straight away: the
/// test server below expects the same prior knowledge, with no ALPN or
/// `Upgrade:` dance. The production client already carries
/// `retry(retry::never())`, so these tests exercise the same classification
/// real callers get, not a test-only stand-in for it.
fn quick() -> RotatingClientBuilder {
    RotatingClient::builder()
        .backoff(Duration::from_millis(5), Duration::from_millis(20))
        .configure(reqwest::ClientBuilder::http2_prior_knowledge)
}

/// What the server does with one stream, in the order [`h2_server`] is
/// given them.
enum StreamAction {
    Reset(Reason),
    Ok(&'static str),
}

/// Answers one stream with a `200` carrying `body`.
fn answer_ok(mut respond: h2::server::SendResponse<Bytes>, body: &'static [u8]) {
    let response = http::Response::builder()
        .status(200)
        .body(())
        .expect("build the response");

    if let Ok(mut send) = respond.send_response(response, false) {
        let _ = send.send_data(Bytes::from_static(body), true);
    }
}

/// A local HTTP/2 server on a random port, accepting a single TCP
/// connection: every attempt in a test is expected to reuse it, so a
/// stream index and a connection index are the same thing here. Each
/// accepted stream is handled with the next [`StreamAction`] from
/// `script`; a stream past the end of the script is reset with
/// `INTERNAL_ERROR`. Returns the base URL and a counter of streams the
/// server accepted.
async fn h2_server(script: Vec<StreamAction>) -> (String, Arc<AtomicUsize>) {
    let (listener, url, seen) = common::bind_counted("http").await;
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
                Some(StreamAction::Ok(body)) => answer_ok(respond, body.as_bytes()),
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
async fn zero_retries_sends_a_post_exactly_once_on_a_refused_stream() {
    let (url, seen) = h2_server(vec![
        StreamAction::Reset(Reason::REFUSED_STREAM),
        StreamAction::Reset(Reason::REFUSED_STREAM),
        StreamAction::Reset(Reason::REFUSED_STREAM),
    ])
    .await;

    let client = quick().retries(0).build().unwrap();
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
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

/// A policy set in `configure()` runs after the crate's own
/// `retry(retry::never())` and replaces it, so reqwest's transparent replay
/// comes back even with this crate's own `retries(0)`.
#[tokio::test]
async fn configure_can_bring_reqwests_own_retries_back() {
    let (url, seen) = h2_server(vec![
        StreamAction::Reset(Reason::REFUSED_STREAM),
        StreamAction::Ok("ok"),
    ])
    .await;

    let client = RotatingClient::builder()
        .backoff(Duration::from_millis(5), Duration::from_millis(20))
        .configure(|b| {
            b.http2_prior_knowledge().retry(
                reqwest::retry::for_host("127.0.0.1")
                    .no_budget()
                    .classify_fn(|req_rep| {
                        if req_rep.error().is_some() {
                            req_rep.retryable()
                        } else {
                            req_rep.success()
                        }
                    }),
            )
        })
        .retries(0)
        .build()
        .unwrap();

    let response = client
        .request(reqwest::Method::POST, &url)
        .body("payload=1")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
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

/// An empty `SETTINGS` frame: the whole server preface.
const EMPTY_SETTINGS: [u8; 9] = [0, 0, 0, 0x4, 0, 0, 0, 0, 0];

/// Header of a `GOAWAY` frame on stream 0: an 8-byte payload (last stream
/// id, then error code), type `0x7`, no flags.
const GOAWAY_HEADER: [u8; 9] = [0, 0, 8, 0x7, 0, 0, 0, 0, 0];

/// Sends the server preface and `GOAWAY(reason, last_stream_id=0)` by hand
/// rather than through h2's `abrupt_shutdown`, which closes the socket as
/// soon as that frame is flushed. Since tokio 1.54 an accepted socket is
/// writable at once, so that close can land before the client has written
/// its request; the client's write then fails with a broken pipe and it
/// never reads the `GOAWAY`. Reading until the client hangs up keeps the
/// socket open, which makes the `GOAWAY` the first thing the client can
/// learn, whichever side runs first.
async fn goaway_and_hold(mut socket: tokio::net::TcpStream, reason: Reason) {
    let mut frames = EMPTY_SETTINGS.to_vec();
    frames.extend_from_slice(&GOAWAY_HEADER);
    frames.extend_from_slice(&0u32.to_be_bytes());
    frames.extend_from_slice(&u32::from(reason).to_be_bytes());

    if socket.write_all(&frames).await.is_err() {
        return;
    }

    let mut sink = [0u8; 1024];
    while matches!(socket.read(&mut sink).await, Ok(n) if n > 0) {}
}

/// A server whose first connection gets a `GOAWAY(reason,
/// last_stream_id=0)` before a single frame is read off it, leaving the
/// request's own stream unprocessed; every later connection answers `200`.
/// Returns the base URL and a counter of TCP connections accepted, so a
/// test can tell whether a retry opened a second one.
async fn goaway_then_ok_server(reason: Reason) -> (String, Arc<AtomicUsize>) {
    let (listener, url, connections) = common::bind_counted("http").await;
    let counter = Arc::clone(&connections);

    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };

            let connection_number = counter.fetch_add(1, Ordering::SeqCst) + 1;

            if connection_number == 1 {
                tokio::spawn(goaway_and_hold(socket, reason));
                continue;
            }

            let Ok(mut conn) = h2::server::handshake(socket).await else {
                continue;
            };

            while let Some(Ok((_request, respond))) = conn.accept().await {
                answer_ok(respond, b"ok");
            }
        }
    });

    (url, connections)
}

/// The `GET` half of the pair below: the first connection's
/// `GOAWAY(NO_ERROR)` leaves the request's own stream unprocessed, so the
/// retry goes out on a fresh connection. Nothing here depends on the
/// method, which is what the `POST` test next to it pins. A `GET` is also
/// replayed after a plain broken pipe, so the retry reason is checked too:
/// it must be the `GOAWAY` (`NeverSent`), not a transport error.
#[tokio::test]
async fn get_is_retried_after_an_unprocessed_goaway() {
    let (url, connections) = goaway_then_ok_server(Reason::NO_ERROR).await;

    let (hook, events) = common::record_retries();
    let client = quick().retries(1).on_retry(hook).build().unwrap();
    let response = client.get(&url).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(connections.load(Ordering::SeqCst), 2);

    let reasons: Vec<RetryReason> = events.lock().unwrap().iter().map(|e| e.reason).collect();
    assert_eq!(reasons, [RetryReason::NeverSent]);
}

/// Same `GOAWAY(NO_ERROR)`, but for a `POST`: the request's own stream was
/// never processed, so replaying it is as safe as a `REFUSED_STREAM` is,
/// and this crate retries it on a fresh connection for every method.
#[tokio::test]
async fn post_is_retried_after_an_unprocessed_goaway() {
    let (url, connections) = goaway_then_ok_server(Reason::NO_ERROR).await;

    let client = quick().retries(1).build().unwrap();
    let response = client
        .request(reqwest::Method::POST, &url)
        .body("payload=1")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

/// An error-code `GOAWAY` is an ordinary transport error: no replay for `POST`.
/// The error must be the remote `GOAWAY(INTERNAL_ERROR)` itself; a broken pipe
/// would also pass the other asserts without the `GOAWAY` ever being read.
#[tokio::test]
async fn post_is_not_retried_after_a_goaway_with_an_error_code() {
    let (url, connections) = goaway_then_ok_server(Reason::INTERNAL_ERROR).await;

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

    let h2_err = std::iter::successors(std::error::Error::source(&inner), |e| e.source())
        .find_map(|e| e.downcast_ref::<h2::Error>())
        .unwrap_or_else(|| panic!("no h2 error in {inner:?}"));
    assert!(h2_err.is_go_away() && h2_err.is_remote(), "{h2_err:?}");
    assert_eq!(h2_err.reason(), Some(Reason::INTERNAL_ERROR));
}
