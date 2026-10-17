//! Shared helpers for the integration tests, split by topic into the other
//! files under `tests/`. Most tests run against `wiremock`, with a few raw
//! TCP listeners for things it cannot stand in for (a connection dropped
//! mid-request, an HTTP proxy answering `407`, a server that takes the
//! request and then stalls). Proxy rotation and cooldown arithmetic are
//! unit-tested directly on `ProxyList` in `src/proxy.rs`.
//!
//! These tests run on real time, not `start_paused`: the client has a
//! request timeout, and tokio's auto-advancing paused clock would fire that
//! timer the moment a task blocks on real socket I/O.
//!
//! Not every helper here is used by every file that declares `mod common;`.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest_rotate::{RetryEvent, RotatingClient, RotatingClientBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOCAL_ANY_PORT: &str = "127.0.0.1:0";
const READ_BUF_LEN: usize = 4096;
const BODY_CHUNK_LEN: usize = 64 * 1024;
const KEEP_ALIVE_OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

pub(crate) const OK_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";

/// Body size for [`keep_alive_503_server`]: below the client's drain
/// budget, but well above what `hyper` buffers along with the headers, so
/// a body that is *not* drained really does cost the connection.
pub(crate) const KEEP_ALIVE_503_BODY_LEN: usize = 48 * 1024;

static BODY_CHUNK: [u8; BODY_CHUNK_LEN] = [b'x'; BODY_CHUNK_LEN];

/// Binds a listener on a random local port and returns it with its
/// `scheme://addr` base URL.
async fn bind_local(scheme: &str) -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind(LOCAL_ANY_PORT)
        .await
        .expect("bind the mock listener");
    let addr = listener.local_addr().expect("read the listener address");

    (listener, format!("{scheme}://{addr}"))
}

/// [`bind_local`] plus a counter starting at zero, for servers that report
/// how many connections, streams or bytes they saw.
pub(crate) async fn bind_counted(
    scheme: &str,
) -> (tokio::net::TcpListener, String, Arc<AtomicUsize>) {
    let (listener, url) = bind_local(scheme).await;

    (listener, url, Arc::new(AtomicUsize::new(0)))
}

/// Accepts the next connection and reads its request once; `None` once
/// accepting fails.
async fn accept_request(listener: &tokio::net::TcpListener) -> Option<tokio::net::TcpStream> {
    let (mut socket, _) = listener.accept().await.ok()?;
    let mut buf = [0u8; READ_BUF_LEN];

    let _ = socket.read(&mut buf).await;

    Some(socket)
}

/// Keeps the socket open, body unfinished, until the test's runtime shuts
/// down.
fn hold_open(socket: tokio::net::TcpStream) {
    tokio::spawn(async move {
        let _socket = socket;

        std::future::pending::<()>().await;
    });
}

/// Builder with tiny backoff so retry tests don't sit around.
pub(crate) fn quick() -> RotatingClientBuilder {
    RotatingClient::builder().backoff(Duration::from_millis(5), Duration::from_millis(20))
}

/// An `on_retry` hook that records every event it sees, and the handle to
/// read them back once the request is done.
pub(crate) fn record_retries() -> (
    impl Fn(&RetryEvent) + Send + Sync + 'static,
    Arc<Mutex<Vec<RetryEvent>>>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let hook = move |event: &RetryEvent| {
        recorded
            .lock()
            .expect("lock the recorded events")
            .push(event.clone());
    };

    (hook, events)
}

/// A minimal HTTP server on a random port. Each connection is read once,
/// then either dropped without a reply (the first `drop_first`
/// connections) or answered with `response`. Returns the base URL and a
/// counter of accepted connections.
pub(crate) async fn raw_server(
    drop_first: usize,
    response: &'static [u8],
) -> (String, Arc<AtomicUsize>) {
    let (listener, url, connections) = bind_counted("http").await;
    let counter = Arc::clone(&connections);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };

            let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let mut buf = [0u8; READ_BUF_LEN];

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

/// Reads the request, sends a `503` declaring `body_len` bytes, then streams
/// the body until the client goes away, adding every chunk the kernel
/// accepted to `written`.
async fn stream_503_body(
    mut socket: tokio::net::TcpStream,
    body_len: usize,
    written: Arc<AtomicUsize>,
) {
    let mut buf = [0u8; READ_BUF_LEN];

    let _ = socket.read(&mut buf).await;

    let head = format!("HTTP/1.1 503 Service Unavailable\r\ncontent-length: {body_len}\r\n\r\n");

    if socket.write_all(head.as_bytes()).await.is_err() {
        return;
    }

    let mut left = body_len;

    while left > 0 {
        let chunk_len = left.min(BODY_CHUNK_LEN);

        if socket.write_all(&BODY_CHUNK[..chunk_len]).await.is_err() {
            break;
        }

        written.fetch_add(chunk_len, Ordering::SeqCst);
        left -= chunk_len;
    }
}

/// A server that answers every connection with a `503` carrying a
/// `body_len`-byte body, streamed in 64 KiB chunks until the client goes
/// away. Returns the base URL and a counter of body bytes the kernel
/// accepted for sending, i.e. roughly what the client actually read.
pub(crate) async fn big_error_body_server(body_len: usize) -> (String, Arc<AtomicUsize>) {
    let (listener, url, written) = bind_counted("http").await;
    let counter = Arc::clone(&written);

    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };

            tokio::spawn(stream_503_body(socket, body_len, Arc::clone(&counter)));
        }
    });

    (url, written)
}

/// Answers every request read from `socket` with `reply` until the client
/// closes the connection or a write fails.
async fn answer_every_request(mut socket: tokio::net::TcpStream, reply: &[u8]) {
    let mut buf = [0u8; READ_BUF_LEN];

    while matches!(socket.read(&mut buf).await, Ok(read_len) if read_len > 0) {
        if socket.write_all(reply).await.is_err() {
            break;
        }
    }
}

/// A keep-alive server that answers every request on a connection with
/// the same `503` and a [`KEEP_ALIVE_503_BODY_LEN`]-byte body. Returns the
/// base URL and a counter of accepted connections, so a test can tell
/// reuse from reconnecting.
pub(crate) async fn keep_alive_503_server() -> (String, Arc<AtomicUsize>) {
    let (listener, url, connections) = bind_counted("http").await;
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
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };

            counter.fetch_add(1, Ordering::SeqCst);

            let reply = Arc::clone(&reply);

            tokio::spawn(async move { answer_every_request(socket, &reply).await });
        }
    });

    (url, connections)
}

/// A server that answers the first request with `503` headers and one body
/// byte of a declared 1024, then stalls; every later request gets `200`.
/// Returns the base URL and when the second request arrived.
pub(crate) async fn stalled_503_body_server() -> (String, Arc<Mutex<Option<std::time::Instant>>>) {
    let (listener, url) = bind_local("http").await;
    let second = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&second);

    tokio::spawn(async move {
        let mut first = true;

        while let Some(mut socket) = accept_request(&listener).await {
            if first {
                first = false;
                let _ = socket
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 1024\r\n\r\nx")
                    .await;
                hold_open(socket);
            } else {
                *seen.lock().expect("lock the arrival time") = Some(std::time::Instant::now());
                let _ = socket.write_all(OK_RESPONSE).await;
            }
        }
    });

    (url, second)
}

/// A server that answers the first request with `503` headers declaring a
/// `content_len`-byte body, then stalls before sending any of it; every
/// later request gets `200`.
pub(crate) async fn stalled_body_of_length_server(content_len: usize) -> String {
    let (listener, url) = bind_local("http").await;

    tokio::spawn(async move {
        let mut first = true;

        while let Some(mut socket) = accept_request(&listener).await {
            if first {
                let head = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-length: {content_len}\r\n\r\n"
                );

                first = false;
                let _ = socket.write_all(head.as_bytes()).await;
                hold_open(socket);
            } else {
                let _ = socket.write_all(OK_RESPONSE).await;
            }
        }
    });

    url
}

/// A server that accepts a connection, reads the request, waits `delay`,
/// then closes the connection without ever answering: stands in for a
/// proxy that took the connection but stalled, the way a per-attempt
/// timeout is meant to catch.
pub(crate) async fn slow_server(delay: Duration) -> String {
    let (listener, url) = bind_local("http").await;

    tokio::spawn(async move {
        while let Some(socket) = accept_request(&listener).await {
            tokio::time::sleep(delay).await;
            drop(socket);
        }
    });

    url
}

/// A SOCKS5 proxy that takes any client without authentication, grants
/// every `CONNECT`, and then answers the tunnelled HTTP request itself
/// with `response`, standing in for the origin. Returns the proxy URL and
/// a counter of accepted connections.
#[cfg(feature = "socks")]
pub(crate) async fn socks_origin(response: &'static [u8]) -> (String, Arc<AtomicUsize>) {
    let (listener, url, connections) = bind_counted("socks5h").await;
    let counter = Arc::clone(&connections);

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);

            let mut buf = [0u8; READ_BUF_LEN];

            // Greeting: pick "no authentication".
            if socket.read(&mut buf).await.is_err() {
                continue;
            }

            let _ = socket.write_all(&[5, 0]).await;

            // CONNECT: grant it with an all-zero bound address.
            if socket.read(&mut buf).await.is_err() {
                continue;
            }

            let _ = socket.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await;

            // The HTTP request itself, answered as the origin.
            let _ = socket.read(&mut buf).await;

            let _ = socket.write_all(response).await;
            let _ = socket.shutdown().await;
        }
    });

    (url, connections)
}

/// A keep-alive server answering every request on a connection with `200`.
/// Returns the base URL and a counter of accepted connections, so a test
/// can tell reuse from reconnecting.
pub(crate) async fn keep_alive_ok_server() -> (String, Arc<AtomicUsize>) {
    let (listener, url, connections) = bind_counted("http").await;
    let counter = Arc::clone(&connections);

    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };

            counter.fetch_add(1, Ordering::SeqCst);

            tokio::spawn(answer_every_request(socket, KEEP_ALIVE_OK_RESPONSE));
        }
    });

    (url, connections)
}
