//! The main [`RotatingClient`] and its builder.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Request, Response};

use crate::error::Error;
use crate::proxy::ProxyList;
use crate::rate_limit::RateLimiter;
#[cfg(feature = "tracing")]
use crate::retry::error_kind;
use crate::retry::{
    backoff_delay, is_idempotent, is_proxy_failure_status, is_retryable_status, is_transport_error,
    retry_after, should_retry_error,
};
use crate::trace_log;

const DEFAULT_RETRIES: u32 = 3;
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(200);
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
const DEFAULT_PROXY_COOLDOWN: Duration = Duration::from_secs(60);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest backoff base, backoff cap or `Retry-After` bound accepted.
/// Anything above it (only absurd values such as `Duration::MAX`) is
/// treated as this, so an oversized base, cap or bound can't turn a retry
/// into a wait nobody will ever see.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// Roughly how much of a retryable response's body is read before the
/// retry, so the connection can go back to the pool. Reading stops after
/// the chunk that crosses this budget, so the actual count can run a
/// little over. A body that does not end within the budget is dropped
/// along with its connection: a reconnect on HTTP/1, a reset stream on
/// HTTP/2, and no buffering either way.
const DRAIN_BUDGET: usize = 64 * 1024;

/// Hook that lets callers apply their own `reqwest::ClientBuilder`
/// settings. Called once per underlying client (one direct, one per proxy).
type ConfigureFn = dyn Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync;

/// An HTTP client that rotates across a pool of proxies, rate-limits
/// requests per host, and retries transient failures with backoff.
///
/// Build one with [`RotatingClient::builder`]. Proxies are optional: with
/// none configured, `RotatingClient` behaves as a plain rate-limited,
/// retrying client that ignores proxy environment variables. See the
/// crate-level docs for what is retried and for a full example.
///
/// Cloning is cheap (an `Arc` bump) and clones share everything:
/// connection pools, proxy cooldown state, and the rate limiter.
#[derive(Clone, Debug)]
pub struct RotatingClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Client used when no proxy is picked for an attempt.
    direct_client: reqwest::Client,
    /// One pre-built client per proxy, parallel to `proxies.as_slice()`.
    proxy_clients: Vec<reqwest::Client>,
    proxies: ProxyList,
    rate_limiter: RateLimiter,
    retries: u32,
    backoff_base: Duration,
    backoff_max: Duration,
    max_retry_after: Duration,
    proxy_cooldown: Duration,
}

impl RotatingClient {
    /// Starts building a [`RotatingClient`].
    ///
    /// # Examples
    ///
    /// ```
    /// use reqwest_rotate::RotatingClient;
    /// use std::time::Duration;
    ///
    /// let client = RotatingClient::builder()
    ///     .rate_limit(Duration::from_millis(200))
    ///     .retries(2)
    ///     .build()
    ///     .unwrap();
    /// # let _ = client;
    /// ```
    pub fn builder() -> RotatingClientBuilder {
        RotatingClientBuilder::default()
    }

    /// Sends a `GET` request to `url`, applying rate limiting, proxy
    /// rotation, and retries.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() -> Result<(), reqwest_rotate::Error> {
    /// use reqwest_rotate::RotatingClient;
    ///
    /// let client = RotatingClient::builder().build()?;
    /// let response = client.get("https://example.com").await?;
    /// println!("status: {}", response.status());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get(&self, url: impl reqwest::IntoUrl) -> Result<Response, Error> {
        let request = self.inner.direct_client.get(url).build()?;
        self.send_with_retry(request).await
    }

    /// Starts building a request with an arbitrary method, using this
    /// client's configuration (headers such as `User-Agent`). Send the
    /// result through [`send`](Self::send) to get rate limiting, proxy
    /// rotation, and retries; or call
    /// [`.build()`](reqwest::RequestBuilder::build) yourself and pass the
    /// result to [`execute`](Self::execute). Calling `.send()` on the
    /// returned builder yourself bypasses rotation, rate limiting and
    /// retries. Pass it to [`send`](Self::send) instead.
    pub fn request(
        &self,
        method: reqwest::Method,
        url: impl reqwest::IntoUrl,
    ) -> reqwest::RequestBuilder {
        self.inner.direct_client.request(method, url)
    }

    /// Builds `request_builder` and sends it, applying rate limiting, proxy
    /// rotation, and retries. Use [`request`](Self::request) to get a
    /// builder for a method other than `GET`.
    pub async fn send(&self, request_builder: reqwest::RequestBuilder) -> Result<Response, Error> {
        let request = request_builder.build()?;
        self.send_with_retry(request).await
    }

    /// Sends a pre-built [`reqwest::Request`], applying rate limiting,
    /// proxy rotation, and retries.
    pub async fn execute(&self, request: Request) -> Result<Response, Error> {
        self.send_with_retry(request).await
    }

    /// Returns the [`ProxyList`] this client rotates over: e.g. to inspect
    /// or react to which proxies are currently in cooldown.
    ///
    /// Calling `pick()` on the returned list advances this client's
    /// rotation and clears an expired cooldown; `as_slice()`, `len()` and
    /// `in_cooldown()` are the read-only accessors.
    pub fn proxies(&self) -> &ProxyList {
        &self.inner.proxies
    }

    /// Shared retry loop used by [`get`](Self::get), [`send`](Self::send),
    /// and [`execute`](Self::execute).
    ///
    /// Retrying a request means resending the same body, which requires
    /// cloning it ([`Request::try_clone`]); that only fails for a streaming
    /// body. The request is cloned on every attempt except the last, where
    /// the original is sent directly. If a clone is needed but fails, the
    /// original is sent once and that attempt is treated as the last one:
    /// the body can't be replayed, but it can at least be sent.
    async fn send_with_retry(&self, request: Request) -> Result<Response, Error> {
        let inner = &*self.inner;
        let idempotent = is_idempotent(request.method());
        let mut pending = Some(request);
        let mut attempt: u32 = 0;

        loop {
            let mut is_last_attempt = attempt >= inner.retries;
            let current = if is_last_attempt {
                pending
                    .take()
                    .expect("request is kept until the last attempt")
            } else {
                match pending
                    .as_ref()
                    .expect("request is kept until the last attempt")
                    .try_clone()
                {
                    Some(clone) => clone,
                    None => {
                        is_last_attempt = true;
                        pending
                            .take()
                            .expect("request is kept until the last attempt")
                    }
                }
            };

            inner
                .rate_limiter
                .wait(current.url().host_str().unwrap_or(""))
                .await;

            let proxy_idx = inner.proxies.pick_index();
            let client = match proxy_idx {
                Some(idx) => &inner.proxy_clients[idx],
                None => &inner.direct_client,
            };
            trace_log!(
                "attempt {attempt} url={} proxy={:?}",
                crate::proxy::log_url(current.url()),
                proxy_idx.map(|idx| inner.proxies.redacted(idx))
            );

            // When a proxy is to blame and another one is out of cooldown,
            // the next attempt goes through a different machine: nothing to
            // wait for. Excluding the one that just failed matters at a
            // zero cooldown, where it would otherwise count as its own
            // healthy alternative. With no other proxy available, retries
            // are paced by the backoff like the single-proxy case, instead
            // of hammering a dead endpoint back to back.
            let switch_delay = |attempt: u32, idx: usize| {
                if inner.proxies.any_healthy_except(idx) {
                    Duration::ZERO
                } else {
                    backoff_delay(attempt, inner.backoff_base, inner.backoff_max)
                }
            };

            match client.execute(current).await {
                Ok(response) => {
                    let status = response.status();
                    let proxy_issue = proxy_idx.is_some() && is_proxy_failure_status(status);
                    if let (true, Some(idx)) = (proxy_issue, proxy_idx) {
                        trace_log!(
                            "proxy {} answered {status}: cooling it down",
                            inner.proxies.redacted(idx)
                        );
                        inner.proxies.mark_bad_index(idx, inner.proxy_cooldown);
                    }
                    if is_last_attempt || !(proxy_issue || is_retryable_status(status, idempotent))
                    {
                        return Ok(response);
                    }

                    let delay = if let (true, Some(idx)) = (proxy_issue, proxy_idx) {
                        switch_delay(attempt, idx)
                    } else {
                        match retry_after(response.headers()) {
                            Some(asked) if asked > inner.max_retry_after => {
                                trace_log!(
                                    "server asked to wait {asked:?}, above max_retry_after: returning {status}"
                                );
                                return Ok(response);
                            }
                            Some(asked) => asked,
                            None => backoff_delay(attempt, inner.backoff_base, inner.backoff_max),
                        }
                    };
                    trace_log!("retrying after {delay:?}, status={status}");
                    drain(response).await;
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
                Err(err) => {
                    // A transport failure seen through a proxy is the
                    // proxy's fault, whether or not this request can be
                    // retried.
                    let proxy_failed = proxy_idx.is_some() && is_transport_error(&err);
                    if let (true, Some(idx)) = (proxy_failed, proxy_idx) {
                        trace_log!(
                            "proxy {} failed ({}): cooling it down",
                            inner.proxies.redacted(idx),
                            error_kind(&err)
                        );
                        inner.proxies.mark_bad_index(idx, inner.proxy_cooldown);
                    }
                    if is_last_attempt || !should_retry_error(&err, idempotent) {
                        return Err(Error::Reqwest(err));
                    }

                    let delay = if let (true, Some(idx)) = (proxy_failed, proxy_idx) {
                        switch_delay(attempt, idx)
                    } else {
                        backoff_delay(attempt, inner.backoff_base, inner.backoff_max)
                    };
                    trace_log!("retrying after {delay:?} ({} error)", error_kind(&err));
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }

            attempt = attempt.saturating_add(1);
        }
    }
}

/// Reads roughly [`DRAIN_BUDGET`] bytes of a response body that is about
/// to be retried. A body that ends within the budget hands its connection
/// back to the pool; a longer one is dropped mid-stream, which costs a
/// reconnect on HTTP/1 or a reset stream on HTTP/2.
async fn drain(mut response: Response) {
    let mut budget = DRAIN_BUDGET;
    while budget > 0 {
        match response.chunk().await {
            Ok(Some(chunk)) => budget = spend(budget, chunk.len()),
            _ => break,
        }
    }
}

/// Charges one chunk against the drain budget; an empty chunk still costs
/// one unit, so a peer sending nothing but empty frames can't loop forever.
fn spend(budget: usize, chunk_len: usize) -> usize {
    budget.saturating_sub(chunk_len.max(1))
}

/// Builder for [`RotatingClient`]. Construct one with
/// [`RotatingClient::builder`].
#[derive(Default)]
pub struct RotatingClientBuilder {
    proxies: Vec<String>,
    proxy_list: Option<ProxyList>,
    rate_limit: Option<Duration>,
    retries: Option<u32>,
    backoff_base: Option<Duration>,
    backoff_max: Option<Duration>,
    max_retry_after: Option<Duration>,
    proxy_cooldown: Option<Duration>,
    user_agent: Option<String>,
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
    configure: Option<Box<ConfigureFn>>,
}

impl fmt::Debug for RotatingClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RotatingClientBuilder")
            .field(
                "proxies",
                &self
                    .proxies
                    .iter()
                    .map(|url| crate::proxy::redact_userinfo(url))
                    .collect::<Vec<_>>(),
            )
            .field("proxy_list", &self.proxy_list)
            .field("rate_limit", &self.rate_limit)
            .field("retries", &self.retries)
            .field("backoff_base", &self.backoff_base)
            .field("backoff_max", &self.backoff_max)
            .field("max_retry_after", &self.max_retry_after)
            .field("proxy_cooldown", &self.proxy_cooldown)
            .field("user_agent", &self.user_agent)
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("configure", &self.configure.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl RotatingClientBuilder {
    /// Sets the proxy pool to rotate over: `"http://user:pass@host:port"`
    /// entries, `"https://..."`, a bare `"host:port"` (treated as HTTP), or
    /// `"socks5://..."` with the `socks` feature. Leave it unset (the
    /// default) to send requests directly, with no proxy. Ignored if
    /// [`proxy_list`](Self::proxy_list) is also set.
    ///
    /// Proxies are only ever taken from here: the `HTTP_PROXY`,
    /// `HTTPS_PROXY` and `ALL_PROXY` environment variables that a bare
    /// `reqwest::Client` picks up are ignored. Pass them explicitly if you
    /// want them.
    ///
    /// Each proxy gets its own underlying `reqwest::Client`, built eagerly
    /// with its own connection pool and TLS configuration. For pools of
    /// hundreds of proxies, share one TLS config across them via
    /// [`configure`](Self::configure) and
    /// [`use_preconfigured_tls`](reqwest::ClientBuilder::use_preconfigured_tls).
    pub fn proxies<I, S>(mut self, proxies: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.proxies = proxies.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the proxy pool directly from a pre-built [`ProxyList`], e.g.
    /// one you validated up front or already put some proxies on cooldown
    /// in. Overrides [`proxies`](Self::proxies) if both are set.
    pub fn proxy_list(mut self, proxy_list: ProxyList) -> Self {
        self.proxy_list = Some(proxy_list);
        self
    }

    /// Minimum interval between two requests to the same host name (port
    /// and scheme are not part of the key). Unset by default, meaning no
    /// rate limiting; zero disables it too. An interval over a year is
    /// capped there.
    ///
    /// Every attempt, retries included, waits its turn: a call that
    /// retries twice takes three slots.
    ///
    /// The limiter sees the host of the URL you request. Redirects are
    /// followed inside `reqwest`, so a redirect to another host is not
    /// rate-limited separately.
    ///
    /// A call cancelled while it is queued for a host (a
    /// [`tokio::time::timeout`], say) gives its slot back, unless another
    /// call has already queued behind it.
    pub fn rate_limit(mut self, interval: Duration) -> Self {
        self.rate_limit = Some(interval);
        self
    }

    /// How many retries follow the first try. Default: 3, so up to 4
    /// attempts. `0` disables retries.
    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = Some(retries);
        self
    }

    /// Exponential backoff base delay and the cap applied to it. Default:
    /// 200 ms base, 30 s max, both capped at a year. These pace the delays
    /// this client computes itself; a wait the server asks for in
    /// `Retry-After` is bounded separately by
    /// [`max_retry_after`](Self::max_retry_after).
    pub fn backoff(mut self, base: Duration, max: Duration) -> Self {
        self.backoff_base = Some(base);
        self.backoff_max = Some(max);
        self
    }

    /// Longest `Retry-After` wait that is honoured. Default: 30 s, capped
    /// at a year.
    ///
    /// A `Retry-After` header on a retryable response replaces the computed
    /// backoff delay. If the server asks for more than this, the response
    /// is returned instead of retrying early against its wishes. Check the
    /// status and the header yourself in that case.
    pub fn max_retry_after(mut self, max: Duration) -> Self {
        self.max_retry_after = Some(max);
        self
    }

    /// How long a proxy is skipped after it fails. Default: 60 s. Zero
    /// never takes a proxy out of rotation, but a retry with nowhere else
    /// to go is still paced by the backoff.
    pub fn proxy_cooldown(mut self, cooldown: Duration) -> Self {
        self.proxy_cooldown = Some(cooldown);
        self
    }

    /// `User-Agent` header sent with every request. Unset by default, in
    /// which case `reqwest` sends none.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// Total timeout for one attempt: from starting the request until the
    /// response body is fully read. Default: 30 s. An attempt that times
    /// out before the response headers arrive is retried for idempotent
    /// requests; a `POST` may already be running on the server, so it is
    /// not. A timeout while *you* read the body surfaces from that read.
    ///
    /// This bounds one attempt, not the whole call: with the default four
    /// attempts a `get()` can take up to about two minutes. Wrap the call
    /// in [`tokio::time::timeout`] for a hard overall budget.
    ///
    /// Pass something huge such as `Duration::MAX` to effectively disable
    /// it. Not recommended with proxies: one that accepts the connection
    /// and never answers would then hang a request forever.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Timeout for establishing a TCP connection, to the proxy if one is
    /// used. Default: 10 s.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Applies your own settings to every underlying
    /// [`reqwest::ClientBuilder`] (one direct client plus one per proxy):
    /// default headers, redirect policy, TLS options, and so on. Runs after
    /// this builder's own settings, so it can override them.
    ///
    /// Anything behind a `reqwest` cargo feature (`gzip`, `brotli`,
    /// `cookies`, `json`, ...) needs that feature enabled on *your* `reqwest`
    /// dependency; this crate only turns on `rustls-tls`, `http2` and
    /// `charset`. Once enabled, `gzip`/`brotli` decoding is on by default
    /// in `reqwest` and needs no call here.
    ///
    /// # Examples
    ///
    /// ```
    /// use reqwest_rotate::RotatingClient;
    ///
    /// let client = RotatingClient::builder()
    ///     .configure(|builder| builder.redirect(reqwest::redirect::Policy::none()))
    ///     .build()
    ///     .unwrap();
    /// # let _ = client;
    /// ```
    pub fn configure<F>(mut self, configure: F) -> Self
    where
        F: Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    {
        self.configure = Some(Box::new(configure));
        self
    }

    /// Builds the [`RotatingClient`], constructing one underlying
    /// `reqwest::Client` per configured proxy plus one direct client.
    ///
    /// Returns [`Error::InvalidProxy`] if a proxy URL is blank, cannot be
    /// parsed, or uses an unsupported scheme; or [`Error::Build`] if the
    /// underlying TLS/client setup fails.
    pub fn build(self) -> Result<RotatingClient, Error> {
        let proxies = match self.proxy_list {
            Some(list) => list,
            None => ProxyList::new(&self.proxies)?,
        };
        let timeout = self.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let connect_timeout = self.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);

        let build_client = |proxy_url: Option<&str>| -> Result<reqwest::Client, Error> {
            let mut builder = reqwest::Client::builder()
                .timeout(timeout)
                .connect_timeout(connect_timeout);
            if let Some(user_agent) = &self.user_agent {
                builder = builder.user_agent(user_agent.as_str());
            }
            match proxy_url {
                Some(proxy_url) => {
                    let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| {
                        Error::InvalidProxy(format!(
                            "{}: {e}",
                            crate::proxy::redact_userinfo(proxy_url)
                        ))
                    })?;
                    builder = builder.proxy(proxy);
                }
                // Without this, reqwest would quietly route "direct"
                // requests through HTTP_PROXY / HTTPS_PROXY / ALL_PROXY from
                // the environment, and a 407 from that proxy would look
                // like an origin response.
                None => builder = builder.no_proxy(),
            }
            if let Some(configure) = &self.configure {
                builder = configure(builder);
            }
            builder.build().map_err(Error::Build)
        };

        let direct_client = build_client(None)?;
        let proxy_clients = proxies
            .as_slice()
            .iter()
            .map(|proxy_url| build_client(Some(proxy_url)))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RotatingClient {
            inner: Arc::new(Inner {
                direct_client,
                proxy_clients,
                proxies,
                rate_limiter: RateLimiter::new(self.rate_limit),
                retries: self.retries.unwrap_or(DEFAULT_RETRIES),
                backoff_base: self
                    .backoff_base
                    .unwrap_or(DEFAULT_BACKOFF_BASE)
                    .min(MAX_BACKOFF),
                backoff_max: self
                    .backoff_max
                    .unwrap_or(DEFAULT_BACKOFF_MAX)
                    .min(MAX_BACKOFF),
                max_retry_after: self
                    .max_retry_after
                    .unwrap_or(DEFAULT_MAX_RETRY_AFTER)
                    .min(MAX_BACKOFF),
                proxy_cooldown: self.proxy_cooldown.unwrap_or(DEFAULT_PROXY_COOLDOWN),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_base_and_max_are_clamped_to_a_year() {
        let client = RotatingClient::builder()
            .backoff(Duration::MAX, Duration::MAX)
            .build()
            .unwrap();
        assert_eq!(client.inner.backoff_base, MAX_BACKOFF);
        assert_eq!(client.inner.backoff_max, MAX_BACKOFF);
    }

    #[test]
    fn builder_debug_hides_credentials() {
        let debug = format!(
            "{:?}",
            RotatingClient::builder().proxies([
                "user:pass@proxy.example:3128",
                "http://user:p@ss@proxy.example:3128",
            ])
        );
        assert!(!debug.contains("pass"), "{debug}");
        assert!(!debug.contains("ss@proxy"), "{debug}");
        assert!(debug.contains("***@proxy.example:3128"), "{debug}");
    }

    #[test]
    fn max_retry_after_is_clamped_to_a_year() {
        let client = RotatingClient::builder()
            .max_retry_after(Duration::MAX)
            .build()
            .unwrap();
        assert_eq!(client.inner.max_retry_after, MAX_BACKOFF);
    }

    #[test]
    fn drain_budget_always_shrinks() {
        assert_eq!(spend(10, 0), 9);
        assert_eq!(spend(10, 4), 6);
        assert_eq!(spend(1, 0), 0);
        assert_eq!(spend(3, 10), 0);
    }
}
