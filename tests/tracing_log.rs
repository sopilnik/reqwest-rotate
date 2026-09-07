//! Isolated from `integration.rs` on purpose: `tracing`'s callsite
//! `Interest` cache is process-global, and this test needs a `DEBUG`
//! subscriber to be live for every callsite the attempt path can hit.
//! When it shares a test binary with dozens of other integration tests
//! that run on threads with no subscriber (`NoSubscriber`), a callsite
//! can get cached as "never interested" right after this test installs
//! its subscriber, silently dropping the event it asserts on. Running
//! alone in its own binary means no other thread ever queries the
//! cache first.

#![cfg(feature = "tracing")]

use std::sync::Arc;
use std::time::Duration;

use reqwest_rotate::RotatingClient;

/// Writes into a shared buffer so the test can inspect what a `tracing`
/// subscriber received, instead of relying on stdout capture.
#[derive(Clone, Default)]
struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The attempt line must carry the request URL without its query string
/// (where callers put secrets such as an API key), and no `trace_log!`
/// site may embed a `reqwest::Error`'s own text, which repeats the URL.
#[tokio::test]
async fn tracing_events_carry_no_query_string() {
    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = RotatingClient::builder()
        .backoff(Duration::from_millis(5), Duration::from_millis(20))
        .retries(1)
        .build()
        .unwrap();
    let _ = client
        .get("http://127.0.0.1:1/v1/data?api_key=SUPERSECRET")
        .await;

    let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        output.contains("url=http://127.0.0.1:1/v1/data"),
        "{output}"
    );
    assert!(!output.contains("SUPERSECRET"), "{output}");
}
