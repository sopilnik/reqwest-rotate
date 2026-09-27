mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(feature = "socks")]
use common::socks_origin;
use common::{OK_RESPONSE, keep_alive_ok_server, quick, raw_server, record_retries, slow_server};
use reqwest::StatusCode;
use reqwest_rotate::{Error, ProxyList, RetryReason, RotatingClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn works_without_any_proxies_configured() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .proxies(Vec::<String>::new())
        .build()
        .unwrap();

    let response = client.get(server.uri()).await.unwrap();
    assert!(response.status().is_success());
}

#[tokio::test]
async fn invalid_proxy_url_is_rejected_at_build_time() {
    let err = RotatingClient::builder()
        .proxies(["not a valid proxy url"])
        .build()
        .unwrap_err();

    assert!(matches!(err, Error::InvalidProxy { .. }));
}

#[cfg(not(feature = "socks"))]
#[tokio::test]
async fn socks_proxy_is_rejected_without_the_feature() {
    let err = RotatingClient::builder()
        .proxies(["socks5://127.0.0.1:1080"])
        .build()
        .unwrap_err();

    assert!(matches!(err, Error::InvalidProxy { .. }), "{err}");
}

#[cfg(feature = "socks")]
#[tokio::test]
async fn socks_proxy_is_accepted_with_the_feature() {
    let client = RotatingClient::builder()
        .proxies(["socks5://127.0.0.1:1080"])
        .build()
        .unwrap();

    assert_eq!(client.proxies().len(), 1);
}

/// `send()` takes a builder from any `reqwest::Client`; the request still
/// goes out through this client's proxy.
#[tokio::test]
async fn send_takes_a_builder_from_another_reqwest_client() {
    let (proxy, seen) = raw_server(0, OK_RESPONSE).await;
    let client = RotatingClient::builder()
        .proxies([proxy.as_str()])
        .build()
        .unwrap();

    let foreign = reqwest::Client::new().get("http://example.invalid/page");
    let response = client.send(foreign).await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connect_failure_marks_proxy_bad() {
    // Ports 1 and 2: nothing listens, so the connect fails at once, not by
    // timing out for real.
    let bad_proxy = "http://127.0.0.1:1";
    let good_proxy = "http://127.0.0.1:2";

    let client = RotatingClient::builder()
        .proxies([bad_proxy, good_proxy])
        .retries(0)
        .proxy_cooldown(Duration::from_secs(60))
        .build()
        .unwrap();

    // Single attempt, through the first proxy in rotation; it can't
    // connect, so it should come back as a transport error.
    let result = client.get("http://example.invalid/").await;
    assert!(result.is_err());

    // The client's own proxy list should now skip the bad proxy: the next
    // pick is the good one, not a repeat of the failed one.
    assert!(client.proxies().in_cooldown(bad_proxy));
    assert!(!client.proxies().in_cooldown(good_proxy));
    assert_eq!(client.proxies().pick(), Some("http://127.0.0.1:2/"));
}

#[tokio::test]
async fn dead_pool_backs_off_instead_of_spinning() {
    // Both proxies refuse connections, so after the first two attempts the
    // whole pool is in cooldown: there is no healthy proxy to switch to,
    // and retries must be paced by the backoff like the single-proxy case.
    let client = RotatingClient::builder()
        .proxies(["http://127.0.0.1:1", "http://127.0.0.1:2"])
        .retries(10)
        .backoff(Duration::from_millis(100), Duration::from_millis(100))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let result = client.get("http://example.invalid/").await;
    let elapsed = start.elapsed();

    assert!(result.is_err());
    // Ten full-jitter delays of up to 100 ms: their sum is below 100 ms
    // with probability 1/10!; with no delay at all the call took ~1 ms.
    assert!(
        elapsed >= Duration::from_millis(100),
        "elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn zero_cooldown_single_proxy_still_backs_off() {
    // A zero cooldown means "never skip a proxy", not "the pool is never
    // dead": with a single refusing proxy, the retry must still be paced by
    // the backoff instead of spinning on the same dead proxy at full speed.
    let client = RotatingClient::builder()
        .proxies(["http://127.0.0.1:1"])
        .proxy_cooldown(Duration::ZERO)
        .retries(10)
        .backoff(Duration::from_millis(100), Duration::from_millis(100))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let result = client.get("http://example.invalid/").await;
    let elapsed = start.elapsed();

    assert!(result.is_err());
    // Same 1/10! argument as above.
    assert!(
        elapsed >= Duration::from_millis(100),
        "elapsed = {elapsed:?}"
    );
}

#[tokio::test]
async fn proxy_407_ignores_retry_after() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\nretry-after: 300\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        // A large backoff: switching proxies must not wait for it.
        .backoff(Duration::from_secs(5), Duration::from_secs(5))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let response = client.get("http://example.invalid/page").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown(&auth_proxy));
}

/// Characterises existing behaviour: unlike an origin's `5xx`, a proxy's
/// `407` never reached the origin at all, so it is retried for a `POST`
/// too: the proxy failed, nothing was duplicated.
#[tokio::test]
async fn proxy_407_is_retried_for_a_post() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        .build()
        .unwrap();

    let request_builder = client
        .request(reqwest::Method::POST, "http://example.invalid/page")
        .body("x=1");
    let response = request_builder.send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
}

/// A connect failure proves the proxy never forwarded anything, so a
/// `POST` is replayed through the next proxy exactly like a `GET`.
#[tokio::test]
async fn post_is_retried_after_a_connect_failure_through_a_proxy() {
    let (good_proxy, good_seen) = raw_server(0, OK_RESPONSE).await;

    let client = quick()
        .proxies(["http://127.0.0.1:1", good_proxy.as_str()])
        .retries(1)
        .build()
        .unwrap();
    let response = client
        .request(reqwest::Method::POST, "http://example.invalid/page")
        .body("x=1")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown("http://127.0.0.1:1"));
}

#[tokio::test]
async fn proxy_407_cools_down_and_rotates() {
    let (auth_proxy, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (good_proxy, good_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = quick()
        .proxies([auth_proxy.as_str(), good_proxy.as_str()])
        .retries(1)
        .build()
        .unwrap();

    // The target host never resolves: only a proxy can answer this.
    let response = client.get("http://example.invalid/page").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    assert_eq!(good_seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown(&auth_proxy));
    assert!(!client.proxies().in_cooldown(&good_proxy));
}

#[tokio::test]
async fn switch_proxy_on_429_goes_to_the_next_proxy_at_once() {
    let (limited, limited_seen) = raw_server(
        0,
        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (other, other_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([limited.as_str(), other.as_str()])
        .retries(1)
        .switch_proxy_on_429(true)
        // Only the switch can finish inside the 2 s timeout below: the
        // backoff and the Retry-After above are both far longer.
        .backoff(Duration::from_secs(60), Duration::from_secs(60))
        .max_retry_after(Duration::from_secs(60))
        .build()
        .unwrap();

    let response = tokio::time::timeout(
        Duration::from_secs(2),
        client.get("http://example.invalid/page"),
    )
    .await
    .expect("switching proxies must not wait")
    .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-b");
    assert_eq!(limited_seen.load(Ordering::SeqCst), 1);
    assert_eq!(other_seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&limited));
}

#[tokio::test]
async fn a_429_through_a_proxy_waits_by_default() {
    let (limited, limited_seen) = raw_server(
        0,
        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (other, other_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([limited.as_str(), other.as_str()])
        .retries(1)
        .backoff(Duration::from_secs(5), Duration::from_secs(5))
        .max_retry_after(Duration::from_secs(1))
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let response = client.get("http://example.invalid/page").await.unwrap();

    // The 30 s Retry-After is above the 1 s cap, so the 429 comes back
    // instead of an early retry against the server's wishes.
    assert_eq!(response.status(), 429);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(limited_seen.load(Ordering::SeqCst), 1);
    assert_eq!(other_seen.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn switch_proxy_on_429_needs_another_healthy_proxy() {
    let (limited, limited_seen) = raw_server(
        0,
        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([limited.as_str()])
        .retries(1)
        .switch_proxy_on_429(true)
        .backoff(Duration::from_secs(5), Duration::from_secs(5))
        .max_retry_after(Duration::from_secs(1))
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/page").await.unwrap();

    // No other proxy to switch to, so the usual Retry-After/backoff path
    // applies, and the 30 s ask above the 1 s cap returns the response.
    assert_eq!(response.status(), 429);
    assert_eq!(limited_seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&limited));
}

#[tokio::test]
async fn switch_proxy_on_429_waits_when_the_other_proxy_is_cooling_down() {
    let (limited, limited_seen) = raw_server(
        0,
        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (other, other_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([limited.as_str(), other.as_str()])
        .retries(1)
        .switch_proxy_on_429(true)
        .backoff(Duration::from_secs(5), Duration::from_secs(5))
        .max_retry_after(Duration::from_secs(1))
        .build()
        .unwrap();
    assert!(client.proxies().mark_bad(&other, Duration::from_secs(60)));

    let response = client.get("http://example.invalid/page").await.unwrap();

    // The only other proxy is cooling down, so the usual path applies and
    // the 30 s ask above the 1 s cap returns the 429.
    assert_eq!(response.status(), 429);
    assert_eq!(limited_seen.load(Ordering::SeqCst), 1);
    assert_eq!(other_seen.load(Ordering::SeqCst), 0);
    assert!(!client.proxies().in_cooldown(&limited));
}

#[tokio::test]
async fn switch_proxy_on_429_does_not_go_back_to_a_limited_proxy() {
    const LIMITED_429: &[u8] =
        b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    let (a, a_seen) = raw_server(0, LIMITED_429).await;
    let (b, b_seen) = raw_server(0, LIMITED_429).await;
    let client = RotatingClient::builder()
        .proxies([a.as_str(), b.as_str()])
        .retries(3)
        .switch_proxy_on_429(true)
        .max_retry_after(Duration::from_secs(1))
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/page").await.unwrap();

    // Each proxy was told to wait 30 s. Once both have been tried, the
    // 30 s ask above the 1 s cap returns the response instead of
    // hitting either IP again.
    assert_eq!(response.status(), 429);
    assert_eq!(a_seen.load(Ordering::SeqCst), 1);
    assert_eq!(b_seen.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn switch_proxy_on_429_falls_back_once_every_proxy_is_limited() {
    const LIMITED: &[u8] =
        b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    let (a, a_seen) = raw_server(0, LIMITED).await;
    let (b, b_seen) = raw_server(0, LIMITED).await;
    let (hook, events) = record_retries();
    let client = RotatingClient::builder()
        .proxies([a.as_str(), b.as_str()])
        .retries(2)
        .switch_proxy_on_429(true)
        .backoff(Duration::from_millis(50), Duration::from_millis(50))
        .on_retry(hook)
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/page").await.unwrap();

    // One switch at no delay; then both proxies have answered 429 to this
    // call, and the second retry waits out the backoff instead.
    assert_eq!(response.status(), 429);
    assert_eq!(
        a_seen.load(Ordering::SeqCst) + b_seen.load(Ordering::SeqCst),
        3
    );
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].delay, Duration::ZERO);
    assert!(events[1].delay > Duration::ZERO, "{:?}", events[1].delay);
}

#[tokio::test]
async fn switch_proxy_on_429_does_not_return_to_a_limited_proxy_after_a_407() {
    let (limited, limited_seen) = raw_server(
        0,
        b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (auth, auth_seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (hook, events) = record_retries();
    let client = RotatingClient::builder()
        .proxies([limited.as_str(), auth.as_str()])
        .retries(2)
        .switch_proxy_on_429(true)
        .backoff(Duration::from_millis(50), Duration::from_millis(50))
        .on_retry(hook)
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/page").await.unwrap();

    assert_eq!(response.status(), 429);
    assert_eq!(limited_seen.load(Ordering::SeqCst), 2);
    assert_eq!(auth_seen.load(Ordering::SeqCst), 1);
    let events = events.lock().unwrap();
    // The 407 cooled the only other proxy: going back to the limited one
    // waits out the backoff instead of asking it again at once.
    assert_eq!(
        events[1].reason,
        RetryReason::ProxyStatus(StatusCode::PROXY_AUTHENTICATION_REQUIRED)
    );
    assert!(events[1].delay > Duration::ZERO, "{:?}", events[1].delay);
}

#[tokio::test]
async fn switch_proxy_on_429_leaves_a_503_alone() {
    let (unavailable, unavailable_seen) = raw_server(
        0,
        b"HTTP/1.1 503 Service Unavailable\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let (other, other_seen) = raw_server(
        0,
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nvia-b",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([unavailable.as_str(), other.as_str()])
        .retries(1)
        .switch_proxy_on_429(true)
        .backoff(Duration::from_secs(5), Duration::from_secs(5))
        .max_retry_after(Duration::from_secs(1))
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/page").await.unwrap();

    // Only a 429 switches proxies: the 503's 30 s ask is above the 1 s
    // cap, so it comes back instead of going to the other proxy.
    assert_eq!(response.status(), 503);
    assert_eq!(unavailable_seen.load(Ordering::SeqCst), 1);
    assert_eq!(other_seen.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn switch_proxy_on_429_leaves_the_direct_client_alone() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .expect(1)
        .mount(&server)
        .await;

    // No proxies, so nothing to switch to: the 30 s ask above the 1 s cap
    // returns the 429 exactly as it would with the option off.
    let client = quick()
        .retries(1)
        .switch_proxy_on_429(true)
        .max_retry_after(Duration::from_secs(1))
        .build()
        .unwrap();
    let response = client
        .get(format!("{}/limited", server.uri()))
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
}

#[tokio::test]
async fn a_proxy_that_answers_leaves_cooldown() {
    let (first, first_seen) = raw_server(0, OK_RESPONSE).await;
    let (second, _second_seen) = raw_server(0, OK_RESPONSE).await;

    let pool = ProxyList::new([first.as_str(), second.as_str()]).unwrap();
    // 60s vs. 120s: the first proxy's cooldown ends sooner, so it is
    // unambiguously the fallback pick with both proxies cooling down.
    pool.mark_bad(&first, Duration::from_secs(60));
    pool.mark_bad(&second, Duration::from_secs(120));

    let client = quick().proxy_list(pool).retries(0).build().unwrap();

    // The target host never resolves: only a proxy can answer this.
    let response = client.get("http://example.invalid/x").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(first_seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&first));
    assert!(client.proxies().in_cooldown(&second));
}

#[tokio::test]
async fn mark_good_lets_a_client_use_a_proxy_early() {
    let (first, first_seen) = raw_server(0, OK_RESPONSE).await;
    let (second, second_seen) = raw_server(0, OK_RESPONSE).await;

    let pool = ProxyList::new([first.as_str(), second.as_str()]).unwrap();
    pool.mark_bad(&first, Duration::from_secs(60));
    pool.mark_bad(&second, Duration::from_secs(120));

    let client = quick().proxy_list(pool).retries(0).build().unwrap();
    assert!(client.proxies().mark_good(&second));

    let response = client.get("http://example.invalid/x").await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(first_seen.load(Ordering::SeqCst), 0);
    assert_eq!(second_seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown(&first));
    assert!(!client.proxies().in_cooldown(&second));
}

#[tokio::test]
async fn origin_403_does_not_blame_the_proxy() {
    let (proxy, seen) = raw_server(
        0,
        b"HTTP/1.1 403 Forbidden\r\ncontent-length: 6\r\nconnection: close\r\n\r\ndenied",
    )
    .await;

    let client = quick()
        .proxies([proxy.as_str()])
        .retries(3)
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/secret").await.unwrap();

    assert_eq!(response.status(), 403);
    assert_eq!(response.text().await.unwrap(), "denied");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&proxy));
}

#[tokio::test]
async fn a_per_attempt_timeout_through_a_proxy_marks_it_bad() {
    let slow_proxy = slow_server(Duration::from_millis(600)).await;

    let client = RotatingClient::builder()
        .proxies([slow_proxy.as_str()])
        .timeout(Duration::from_millis(200))
        .retries(0)
        .build()
        .unwrap();

    let err = client.get("http://example.invalid/slow").await.unwrap_err();
    assert!(
        matches!(&err, Error::Reqwest(e) if e.is_timeout()),
        "{err:?}"
    );
    assert!(client.proxies().in_cooldown(&slow_proxy));
}

/// A streaming request body that fails on the caller's own side, mid
/// stream, must not cool down the proxy that carried it: the proxy never
/// gets the chance to fail, since the request never finished leaving.
#[tokio::test]
async fn a_failing_request_body_does_not_blame_the_proxy() {
    let proxy = slow_server(Duration::from_secs(5)).await;
    let client = quick()
        .proxies([proxy.as_str()])
        .retries(0)
        .build()
        .unwrap();
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter([
        Ok::<&'static [u8], std::io::Error>(b"first chunk"),
        Err(std::io::Error::other("the caller's source failed")),
    ]));

    let err = client
        .request(reqwest::Method::PUT, "http://example.invalid/upload")
        .body(body)
        .send()
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Reqwest(_)), "{err}");
    assert!(!client.proxies().in_cooldown(&proxy));
}

#[tokio::test]
async fn https_through_a_proxy_that_refuses_connect_is_a_connect_error() {
    let (proxy, seen) = raw_server(
        0,
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;

    let client = RotatingClient::builder()
        .proxies([proxy.as_str()])
        .retries(0)
        .build()
        .unwrap();

    let err = client
        .get("https://example.invalid/secure")
        .await
        .unwrap_err();

    let Error::Reqwest(inner) = &err else {
        panic!("expected Error::Reqwest, got {err:?}");
    };
    assert!(inner.is_connect(), "{err}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(client.proxies().in_cooldown(&proxy));
}

/// A SOCKS proxy cannot answer in HTTP, so a `407` through one is the
/// origin's, and the origin already has the `POST`: it comes back as it
/// is, is not replayed, and the proxy stays healthy.
#[cfg(feature = "socks")]
#[tokio::test]
async fn a_407_through_a_socks_proxy_is_the_origins_answer() {
    let (proxy, seen) = socks_origin(
        b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
    )
    .await;
    let client = quick()
        .proxies([proxy.as_str()])
        .retries(1)
        .build()
        .unwrap();

    let response = client
        .request(reqwest::Method::POST, "http://example.invalid/page")
        .body("x=1")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 407);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(!client.proxies().in_cooldown(&proxy));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotation_holds_up_under_concurrency() {
    let (proxy_a, seen_a) = raw_server(0, OK_RESPONSE).await;
    let (proxy_b, seen_b) = raw_server(0, OK_RESPONSE).await;

    let client = RotatingClient::builder()
        .proxies([proxy_a.as_str(), proxy_b.as_str()])
        .retries(0)
        .build()
        .unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client.get("http://example.invalid/many").await
        }));
    }
    for task in tasks {
        assert!(task.await.unwrap().is_ok());
    }

    let a = seen_a.load(Ordering::SeqCst);
    let b = seen_b.load(Ordering::SeqCst);
    assert_eq!((a, b), (4, 4), "a={a} b={b}");
}

/// One proxy drops every connection. Pick 0 is always it, and a task's own
/// failure cools it before that task retries, so the counts are exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cooldown_holds_up_under_concurrency() {
    let (dropping, _dropping_seen) = raw_server(usize::MAX, OK_RESPONSE).await;
    let (healthy, healthy_seen) = raw_server(0, OK_RESPONSE).await;

    let client = quick()
        .proxies([dropping.as_str(), healthy.as_str()])
        .retries(1)
        .build()
        .unwrap();

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client.get("http://example.invalid/many").await
        }));
    }
    for task in tasks {
        assert!(task.await.unwrap().is_ok());
    }

    assert_eq!(healthy_seen.load(Ordering::SeqCst), 8);
    assert!(client.proxies().in_cooldown(&dropping));
}

#[tokio::test]
async fn connect_timeout_cuts_off_a_proxy_that_stalls_the_tunnel() {
    let stalling_proxy = slow_server(Duration::from_secs(30)).await;

    let client = RotatingClient::builder()
        .proxies([stalling_proxy.as_str()])
        .connect_timeout(Duration::from_millis(200))
        .timeout(Duration::from_secs(2))
        .retries(0)
        .build()
        .unwrap();

    let start = tokio::time::Instant::now();
    let err = client
        .get("https://example.invalid/secure")
        .await
        .unwrap_err();
    let elapsed = start.elapsed();

    let Error::Reqwest(inner) = &err else {
        panic!("expected Error::Reqwest, got {err:?}");
    };
    assert!(inner.is_connect(), "{err}");
    assert!(elapsed < Duration::from_secs(1), "elapsed = {elapsed:?}");
}

/// An idle proxy connection is closed well before reqwest's own 90 s
/// default would close it.
#[tokio::test]
#[ignore = "sleeps past the pool idle timeout"]
async fn idle_connections_are_closed_after_the_pool_timeout() {
    let (proxy, connections) = keep_alive_ok_server().await;
    let client = RotatingClient::builder()
        .proxies([proxy.as_str()])
        .build()
        .unwrap();

    let response = client.get("http://example.invalid/").await.unwrap();
    assert_eq!(response.status(), 200);

    tokio::time::sleep(Duration::from_secs(16)).await;

    let response = client.get("http://example.invalid/").await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}
