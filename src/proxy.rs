//! Round-robin proxy rotation with a cooldown for proxies that recently
//! failed.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::error::Error;

/// A pool of proxy URLs rotated round-robin, with a cooldown applied to
/// proxies that were recently marked bad (e.g. after a connect failure or a
/// `407` response from the proxy itself).
///
/// URLs are validated and canonicalised on construction: a bare
/// `host:port` becomes `http://host:port/`, scheme and host are lowercased,
/// default ports are dropped, duplicates (after canonicalisation) are
/// removed, and SOCKS schemes are rejected unless the `socks` feature is
/// enabled. [`pick`](Self::pick) and [`as_slice`](Self::as_slice) return
/// the canonical form; [`mark_bad`](Self::mark_bad) and
/// [`in_cooldown`](Self::in_cooldown) accept either form.
///
/// All state lives behind an internal mutex that is never held across an
/// `.await`, so a single `ProxyList` can be used concurrently from many
/// tasks. Its `Debug` output hides proxy credentials.
///
/// # Examples
///
/// ```
/// use reqwest_rotate::ProxyList;
///
/// let proxies = ProxyList::new([
///     "http://proxy-a.example:8080",
///     "proxy-b.example:8080", // no scheme: treated as http://
/// ])
/// .unwrap();
///
/// assert_eq!(proxies.pick(), Some("http://proxy-a.example:8080/"));
/// assert_eq!(proxies.pick(), Some("http://proxy-b.example:8080/"));
/// assert_eq!(proxies.pick(), Some("http://proxy-a.example:8080/"));
/// ```
pub struct ProxyList {
    proxies: Vec<String>,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    next_index: usize,
    /// `bad_until[i]` is the end of proxy `i`'s cooldown, if it is in one.
    bad_until: Vec<Option<Instant>>,
}

impl ProxyList {
    /// Builds a proxy list from proxy URLs such as
    /// `"http://user:pass@host:port"`. An empty iterator is valid and means
    /// "no proxies": [`pick`](Self::pick) then always returns `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidProxy`] if an entry is blank, cannot be
    /// parsed as a URL, or uses an unsupported scheme.
    ///
    /// # Examples
    ///
    /// ```
    /// use reqwest_rotate::ProxyList;
    ///
    /// let empty = ProxyList::new(Vec::<String>::new()).unwrap();
    /// assert!(empty.is_empty());
    /// assert_eq!(empty.pick(), None);
    /// ```
    pub fn new<I, S>(proxies: I) -> Result<Self, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut normalized: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for proxy in proxies {
            let url = normalize_proxy_url(proxy.as_ref())?;
            // Order is the rotation order, so the Vec stays; the set only
            // answers "have I taken this one already" in O(1) instead of
            // scanning.
            if seen.insert(url.clone()) {
                normalized.push(url);
            }
        }
        let bad_until = vec![None; normalized.len()];
        Ok(Self {
            proxies: normalized,
            state: Mutex::new(State {
                next_index: 0,
                bad_until,
            }),
        })
    }

    /// Returns `true` if no proxies were configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.proxies.is_empty()
    }

    /// Number of configured (distinct) proxies, regardless of cooldown state.
    #[must_use]
    pub fn len(&self) -> usize {
        self.proxies.len()
    }

    /// All configured proxy URLs, canonicalised, in rotation order.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.proxies
    }

    /// Index of `proxy`, accepting either the canonical form or anything
    /// that canonicalises to it.
    fn position(&self, proxy: &str) -> Option<usize> {
        self.proxies.iter().position(|p| p == proxy).or_else(|| {
            let canonical = normalize_proxy_url(proxy).ok()?;
            self.proxies.iter().position(|p| *p == canonical)
        })
    }

    /// The proxy at `idx` with any `user:password@` replaced by `***@`,
    /// for logs. Only referenced from `trace_log!` call sites, which
    /// compile away without the `tracing` feature.
    #[cfg_attr(not(feature = "tracing"), allow(dead_code))]
    pub(crate) fn redacted(&self, idx: usize) -> String {
        redact_userinfo(&self.proxies[idx])
    }

    /// Picks the next proxy in round-robin order, skipping proxies that are
    /// still in cooldown when a healthy one is available. Returns `None`
    /// only if the list is empty.
    ///
    /// If every proxy is in cooldown, the one whose cooldown ends soonest
    /// is returned anyway: a proxy that might work beats no proxy at all.
    /// A proxy that then answers a request sent through a
    /// [`RotatingClient`](crate::RotatingClient) that rotates over this
    /// list is taken out of cooldown at once; the others stay marked until
    /// their own cooldown expires.
    pub fn pick(&self) -> Option<&str> {
        self.pick_index().map(|idx| self.proxies[idx].as_str())
    }

    /// Same as [`pick`](Self::pick) but returns the index into
    /// [`as_slice`](Self::as_slice), so the client can map it straight to a
    /// pre-built `reqwest::Client` without hashing or cloning the URL.
    pub(crate) fn pick_index(&self) -> Option<usize> {
        let len = self.proxies.len();
        if len == 0 {
            return None;
        }
        let mut state = self.lock();
        let now = Instant::now();
        let start = state.next_index;

        // One lap looking for a proxy that is not in cooldown.
        for offset in 0..len {
            let idx = (start + offset) % len;
            let healthy = state.bad_until[idx].is_none_or(|until| now >= until);
            if healthy {
                state.bad_until[idx] = None;
                state.next_index = (idx + 1) % len;
                return Some(idx);
            }
        }

        // Everything is cooling down: take the one that recovers first,
        // preferring rotation order on ties.
        let idx = (0..len)
            .map(|offset| (start + offset) % len)
            .min_by_key(|&idx| state.bad_until[idx])
            .expect("len > 0");
        state.next_index = (idx + 1) % len;
        Some(idx)
    }

    /// Whether a proxy other than `idx` is out of cooldown right now, i.e.
    /// whether the next [`pick`](Self::pick) can avoid the one that just
    /// failed. `false` when the list holds no other proxy.
    pub(crate) fn any_healthy_except(&self, idx: usize) -> bool {
        let state = self.lock();
        let now = Instant::now();
        state
            .bad_until
            .iter()
            .enumerate()
            .any(|(i, until)| i != idx && until.is_none_or(|until| now >= until))
    }

    /// Marks a proxy as bad for `cooldown`: [`pick`](Self::pick) will skip
    /// it, unless every proxy is unhealthy, until the cooldown expires or
    /// the proxy answers a request sent through a
    /// [`RotatingClient`](crate::RotatingClient), whichever comes first.
    ///
    /// `proxy` is matched in canonical form, so both what
    /// [`pick`](Self::pick) returned and what you originally configured
    /// work. Returns `false` if it is not in this list, in which case
    /// nothing changes.
    pub fn mark_bad(&self, proxy: &str, cooldown: Duration) -> bool {
        match self.position(proxy) {
            Some(idx) => {
                self.mark_bad_index(idx, cooldown);
                true
            }
            None => false,
        }
    }

    pub(crate) fn mark_bad_index(&self, idx: usize, cooldown: Duration) {
        let now = Instant::now();
        let until = now
            .checked_add(cooldown)
            // A cooldown that overflows `Instant` is capped rather than dropped.
            .or_else(|| now.checked_add(crate::MAX_DURATION))
            .unwrap_or(now);
        self.lock().bad_until[idx] = Some(until);
    }

    /// Takes a proxy out of cooldown: it just answered, so whatever put it
    /// there is stale. Recovery follows evidence rather than the clock.
    pub(crate) fn mark_good_index(&self, idx: usize) {
        self.lock().bad_until[idx] = None;
    }

    /// Returns `true` if `proxy` (in either form, see
    /// [`mark_bad`](Self::mark_bad)) is currently in cooldown.
    pub fn in_cooldown(&self, proxy: &str) -> bool {
        let Some(idx) = self.position(proxy) else {
            return false;
        };
        let state = self.lock();
        state.bad_until[idx].is_some_and(|until| Instant::now() < until)
    }

    /// Locks the state, recovering from a poisoned mutex: the state is a
    /// few integers that are always left consistent, so a panic elsewhere
    /// must not take the whole pool down with it.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for ProxyList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: Vec<String> = self.proxies.iter().map(|p| redact_userinfo(p)).collect();
        let in_cooldown: Vec<&str> = {
            let state = self.lock();
            let now = Instant::now();
            redacted
                .iter()
                .zip(&state.bad_until)
                .filter(|(_, until)| until.is_some_and(|until| now < until))
                .map(|(url, _)| url.as_str())
                .collect()
        };
        f.debug_struct("ProxyList")
            .field("proxies", &redacted)
            .field("in_cooldown", &in_cooldown)
            .finish()
    }
}

/// Replaces `user:password@` in a proxy URL with `***@` so credentials
/// never end up in logs, error messages, or `Debug` output, whether or
/// not the URL has a scheme, and even when the password itself contains
/// `@`.
pub(crate) fn redact_userinfo(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, url),
    };
    // Userinfo can only live in the authority, which ends at the first
    // `/`, `?` or `#`; its separator is the *last* `@` there, because an
    // `@` inside the password is legal input (the url crate escapes it).
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let Some((_, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    match scheme {
        Some(scheme) => format!("{scheme}://***@{host}{tail}"),
        None => format!("***@{host}{tail}"),
    }
}

/// Redacts a proxy spelling that `Url::parse` rejected: the authority's
/// end is unknown, so everything up to the last `@` is treated as
/// credentials.
fn redact_unparsable(raw: &str) -> String {
    let Some(at) = raw.rfind('@') else {
        return raw.to_string();
    };
    let tail = &raw[at + 1..];
    let prefix = raw
        .find("://")
        .filter(|p| *p < at)
        .map(|p| &raw[..p + 3])
        .unwrap_or("");
    format!("{prefix}***@{tail}")
}

/// Validates a proxy URL and brings it into the canonical form `reqwest`
/// expects: lowercase scheme and host, default port dropped, trailing `/`.
///
/// Accepts `scheme://[user:pass@]host[:port]` for the supported schemes,
/// and a bare `[user:pass@]host[:port]`, which gets `http://` prepended.
fn normalize_proxy_url(raw: &str) -> Result<String, Error> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::invalid_proxy(String::new(), "proxy URL is empty"));
    }

    // An explicit `scheme://` is taken at face value (and its scheme
    // checked below). Anything else (`host:port`, `1.2.3.4:8080`,
    // `user:pass@host:port`) is treated as an HTTP proxy. Trying to parse
    // those directly would misread `host` as a scheme, so don't.
    let has_scheme = raw.contains("://");
    let with_scheme = if has_scheme {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    // Never interpolate `raw` itself into an error message below: it may
    // carry `user:pass@`, and errors must not surface proxy credentials
    // any more than Debug output does. Redacting `raw` rather than
    // `with_scheme` keeps the message in the caller's own spelling. Input
    // the parser rejects goes through `redact_unparsable` instead, which
    // assumes the worst about where the credentials end.
    let shown = redact_userinfo(raw);
    let url = reqwest::Url::parse(&with_scheme)
        .ok()
        .filter(reqwest::Url::has_host)
        .ok_or_else(|| {
            let unparsable = redact_unparsable(raw);
            Error::invalid_proxy(unparsable, "not a valid proxy URL")
        })?;

    check_scheme(url.scheme(), &shown)?;
    Ok(url.to_string())
}

fn check_scheme(scheme: &str, shown: &str) -> Result<(), Error> {
    match scheme {
        "http" | "https" => Ok(()),
        "socks4" | "socks4a" | "socks5" | "socks5h" => {
            if cfg!(feature = "socks") {
                Ok(())
            } else {
                Err(Error::invalid_proxy(
                    shown,
                    "SOCKS proxies need the `socks` feature of reqwest-rotate",
                ))
            }
        }
        other => Err(Error::invalid_proxy(
            shown,
            format!("unsupported proxy scheme `{other}`"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(proxies: &[&str]) -> ProxyList {
        ProxyList::new(proxies).unwrap()
    }

    #[test]
    fn round_robin_cycles_through_all_proxies() {
        let list = list(&["http://a", "http://b", "http://c"]);
        assert_eq!(list.pick(), Some("http://a/"));
        assert_eq!(list.pick(), Some("http://b/"));
        assert_eq!(list.pick(), Some("http://c/"));
        assert_eq!(list.pick(), Some("http://a/"));
    }

    #[test]
    fn empty_list_has_no_pick() {
        let list = ProxyList::new(Vec::<String>::new()).unwrap();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert_eq!(list.pick(), None);
    }

    #[test]
    fn rejects_blank_proxy_entries() {
        let err = ProxyList::new(["  "]).unwrap_err();
        assert!(matches!(err, Error::InvalidProxy { .. }));
    }

    #[test]
    fn rejects_unparseable_and_unknown_schemes() {
        assert!(matches!(
            ProxyList::new(["not a valid proxy url"]).unwrap_err(),
            Error::InvalidProxy { .. }
        ));
        assert!(matches!(
            ProxyList::new(["ftp://proxy.example:21"]).unwrap_err(),
            Error::InvalidProxy { .. }
        ));
    }

    #[test]
    fn scheme_less_entries_are_treated_as_http() {
        let list = list(&[
            "1.2.3.4:8080",
            "user:pass@proxy.example:3128",
            "localhost:9",
        ]);
        assert_eq!(
            list.as_slice(),
            &[
                "http://1.2.3.4:8080/",
                "http://user:pass@proxy.example:3128/",
                "http://localhost:9/",
            ]
        );
    }

    #[test]
    fn urls_are_canonicalised() {
        let list = list(&[
            "HTTP://Proxy.Example:80",
            "https://user:pass@proxy.example:443",
            "http://proxy.example:8080/",
        ]);
        assert_eq!(
            list.as_slice(),
            &[
                "http://proxy.example/",
                "https://user:pass@proxy.example/",
                "http://proxy.example:8080/",
            ]
        );
    }

    #[cfg(not(feature = "socks"))]
    #[test]
    fn socks_is_rejected_without_the_feature() {
        let err = ProxyList::new(["socks5://127.0.0.1:1080"]).unwrap_err();
        let Error::InvalidProxy { .. } = &err else {
            panic!("expected InvalidProxy");
        };
        assert!(err.to_string().contains("socks"), "{err}");
    }

    #[cfg(feature = "socks")]
    #[test]
    fn socks_is_accepted_with_the_feature() {
        let list = list(&["socks5://127.0.0.1:1080", "socks5h://127.0.0.1:1081"]);
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn duplicates_are_dropped_keeping_first_position() {
        let list = list(&["http://a", "http://b", "http://a/", "b", "HTTP://A:80"]);
        assert_eq!(list.as_slice(), &["http://a/", "http://b/"]);
    }

    #[test]
    fn dedup_holds_at_scale() {
        // Every second entry repeats the one before it, so 2000 inputs
        // collapse to 1000 distinct proxies. This exercises dedup at a size
        // the other tests never reach; it is a correctness check, not a
        // benchmark (no timing assertion here).
        let proxies: Vec<String> = (0..2000)
            .map(|i| format!("http://10.0.0.{}:{}", i / 2 % 256, 9000 + i / 2))
            .collect();
        let list = ProxyList::new(&proxies).unwrap();
        assert_eq!(list.len(), 1000);
        assert_eq!(list.as_slice()[0], "http://10.0.0.0:9000/");
        assert_eq!(list.as_slice()[999], "http://10.0.0.231:9999/");
    }

    #[test]
    fn mark_bad_is_skipped_until_cooldown_expires() {
        let list = list(&["http://a", "http://b"]);
        assert_eq!(list.pick(), Some("http://a/"));
        // The un-canonicalised spelling is accepted too.
        assert!(list.mark_bad("http://b", Duration::from_millis(50)));
        assert!(list.in_cooldown("http://b/"));
        assert!(list.in_cooldown("b"));
        // "b" is next in rotation but is in cooldown, so "a" is served again.
        assert_eq!(list.pick(), Some("http://a/"));
        std::thread::sleep(Duration::from_millis(80));
        assert!(!list.in_cooldown("http://b/"));
        assert_eq!(list.pick(), Some("http://b/"));
    }

    #[test]
    fn mark_bad_on_unknown_proxy_is_a_no_op() {
        let list = list(&["http://a"]);
        assert!(!list.mark_bad("http://nope", Duration::from_secs(60)));
        assert!(!list.mark_bad("not a url at all", Duration::from_secs(60)));
        assert!(!list.in_cooldown("http://nope"));
        assert_eq!(list.pick(), Some("http://a/"));
    }

    #[test]
    fn mark_good_index_clears_a_cooldown() {
        let list = list(&["http://a", "http://b"]);
        list.mark_bad("http://a", Duration::from_secs(60));
        assert!(list.in_cooldown("http://a"));
        list.mark_good_index(0);
        assert!(!list.in_cooldown("http://a"));
        assert_eq!(list.pick(), Some("http://a/"));
    }

    #[test]
    fn all_proxies_bad_returns_the_one_recovering_first() {
        let list = list(&["http://a", "http://b", "http://c"]);
        list.mark_bad("http://a", Duration::from_secs(60));
        list.mark_bad("http://b", Duration::from_secs(10));
        list.mark_bad("http://c", Duration::from_secs(60));
        assert_eq!(list.pick(), Some("http://b/"));
        // With every proxy cooling down, consecutive picks repeat the
        // soonest-recovering one: nothing here has answered to clear it.
        assert_eq!(list.pick(), Some("http://b/"));
    }

    #[test]
    fn any_healthy_except_ignores_the_given_index() {
        let pool = list(&["http://a", "http://b"]);
        assert!(pool.any_healthy_except(0));
        pool.mark_bad("http://b", Duration::from_secs(60));
        assert!(!pool.any_healthy_except(0));
        assert!(pool.any_healthy_except(1));

        // Once a cooldown has actually expired (not just been set to a
        // duration too short to ever matter), the proxy it covers counts as
        // healthy again: "a" recovers while "b" (excluded above) stays bad.
        pool.mark_bad("http://a", Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(80));
        assert!(pool.any_healthy_except(1));

        let single = list(&["http://a"]);
        assert!(!single.any_healthy_except(0));
    }

    #[test]
    fn huge_cooldown_does_not_panic_or_poison() {
        let list = list(&["http://a", "http://b"]);
        list.mark_bad("http://a", Duration::MAX);
        assert!(list.in_cooldown("http://a"));
        assert_eq!(list.pick(), Some("http://b/"));
    }

    #[test]
    fn debug_output_hides_credentials() {
        let list = list(&["http://user:s3cret@a:8080", "http://b"]);
        list.mark_bad("http://b", Duration::from_secs(60));
        let debug = format!("{list:?}");
        assert!(!debug.contains("s3cret"), "{debug}");
        assert!(debug.contains("http://***@a:8080/"), "{debug}");
        assert!(debug.contains("in_cooldown: [\"http://b/\"]"), "{debug}");
    }

    #[test]
    fn redaction_handles_urls_without_credentials() {
        assert_eq!(redact_userinfo("http://a:8080/"), "http://a:8080/");
        assert_eq!(redact_userinfo("http://u:p@a/"), "http://***@a/");
        assert_eq!(redact_userinfo("socks5://u@a/"), "socks5://***@a/");
        assert_eq!(redact_userinfo("garbage"), "garbage");
        assert_eq!(
            redact_userinfo("user:pass@proxy.example:3128"),
            "***@proxy.example:3128"
        );
        assert_eq!(
            redact_userinfo("http://user:p@ss@proxy.example:3128"),
            "http://***@proxy.example:3128"
        );
        assert_eq!(redact_userinfo("http://h/@path"), "http://h/@path");
        assert_eq!(
            redact_userinfo("http://u:p@h/@path?x=@y"),
            "http://***@h/@path?x=@y"
        );
    }

    #[test]
    fn unparsable_redaction_assumes_the_worst() {
        assert_eq!(
            redact_unparsable("http://user:p?ss@host:3128"),
            "http://***@host:3128"
        );
        assert_eq!(redact_unparsable("user:p#ss@host:3128"), "***@host:3128");
        assert_eq!(redact_unparsable("a@b@c"), "***@c");
        assert_eq!(redact_unparsable("nope"), "nope");
        assert_eq!(redact_unparsable("http://"), "http://");
        assert_eq!(redact_unparsable("user@host://x"), "***@host://x");
    }

    #[test]
    fn invalid_proxy_errors_redact_credentials() {
        let message = ProxyList::new(["ftp://user:pass@host:21"])
            .unwrap_err()
            .to_string();
        assert!(!message.contains("pass"), "{message}");
        assert!(!message.contains("user:"), "{message}");
        assert!(message.contains("***@"), "{message}");

        // A schemeless input must not have the `http://` this function
        // synthesizes internally leak into the message: the caller never
        // typed it, so the message should open with their own spelling.
        let schemeless = ProxyList::new(["not a valid proxy url"])
            .unwrap_err()
            .to_string();
        assert!(
            schemeless.starts_with("invalid proxy: not a valid proxy url:"),
            "{schemeless}"
        );

        let schemeless_with_credentials = ProxyList::new(["user:s3cret@not a url"])
            .unwrap_err()
            .to_string();
        assert!(
            !schemeless_with_credentials.contains("s3cret"),
            "{schemeless_with_credentials}"
        );
        assert!(
            schemeless_with_credentials.starts_with("invalid proxy: ***@"),
            "{schemeless_with_credentials}"
        );

        let scheme_with_at_in_password = ProxyList::new(["ftp://user:p@ss@host:21"])
            .unwrap_err()
            .to_string();
        assert!(
            !scheme_with_at_in_password.contains("ss@"),
            "{scheme_with_at_in_password}"
        );
        assert!(
            scheme_with_at_in_password.contains("***@host"),
            "{scheme_with_at_in_password}"
        );

        let schemeless_with_at_in_password = ProxyList::new(["user:p@ss@host:99999"])
            .unwrap_err()
            .to_string();
        assert!(
            !schemeless_with_at_in_password.contains("ss@"),
            "{schemeless_with_at_in_password}"
        );
        assert!(
            schemeless_with_at_in_password.contains("***@host"),
            "{schemeless_with_at_in_password}"
        );

        // A password containing `/`, `?` or `#` makes the URL fail to
        // parse; the authority boundary is then unknown, so the whole
        // spelling up to the last `@` must be redacted, not just the part
        // `redact_userinfo` would have guessed at.
        let slash_in_password = ProxyList::new(["http://user:p?ss@host:3128"])
            .unwrap_err()
            .to_string();
        assert!(!slash_in_password.contains("p?ss"), "{slash_in_password}");
        assert_eq!(
            slash_in_password,
            "invalid proxy: http://***@host:3128: not a valid proxy URL"
        );

        let hash_in_password = ProxyList::new(["user:p#ss@host:3128"])
            .unwrap_err()
            .to_string();
        assert!(!hash_in_password.contains("p#ss"), "{hash_in_password}");
        assert!(hash_in_password.contains("***@host:3128: not a valid proxy URL"));

        // Guard: input with no `@` at all is unaffected by the redaction
        // change, before or after.
        let no_credentials = ProxyList::new(["http://"]).unwrap_err().to_string();
        assert_eq!(
            no_credentials,
            "invalid proxy: http://: not a valid proxy URL"
        );
    }
}
