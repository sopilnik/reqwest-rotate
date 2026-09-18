//! Its own test binary: `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` are
//! process-global, so a test that sets them cannot share a process with
//! anything else that builds a client and expects its own proxy choices to
//! hold.

use std::time::Duration;

use reqwest_rotate::RotatingClient;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A `RotatingClient` with no proxies configured ignores the environment
/// entirely: with every proxy variable pointed at a port nothing listens
/// on, a plain request to a real server must still succeed. A bare
/// `reqwest::Client` would pick these up and fail to connect.
#[tokio::test]
async fn env_proxy_variables_are_ignored() {
    let unreachable = "http://127.0.0.1:1";
    // SAFETY: nothing else in this process reads the environment while
    // these are set - one test in its own binary, and they are set
    // before it starts a server or a socket. A second test in this
    // file would break that.
    unsafe {
        for var in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            std::env::set_var(var, unreachable);
        }
        for var in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(var);
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let client = RotatingClient::builder()
        .backoff(Duration::from_millis(5), Duration::from_millis(20))
        .retries(0)
        .build()
        .unwrap();
    let response = client.get(server.uri()).await.unwrap();

    assert_eq!(response.status(), 200);
}
