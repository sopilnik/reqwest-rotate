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
/// enabled. [`pick`](Self::pick), [`as_slice`](Self::as_slice) and
/// [`iter`](Self::iter) return the canonical form;
/// [`mark_bad`](Self::mark_bad), [`mark_good`](Self::mark_good) and
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
        let mut canonical: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for proxy in proxies {
            let url = canonical_proxy_url(proxy.as_ref())?;
            // Order is the rotation order, so the Vec stays; the set only
            // answers "have I taken this one already" in O(1) instead of
            // scanning.
            if seen.insert(url.clone()) {
                canonical.push(url);
            }
        }
        let bad_until = vec![None; canonical.len()];
        Ok(Self {
            proxies: canonical,
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
    /// Prefer [`iter`](Self::iter); this stays available until 1.0.
    ///
    /// The URLs keep any `user:password@`, since that is what the client
    /// connects with; strip it before you log or print one, or use
    /// [`iter_redacted`](Self::iter_redacted) instead.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.proxies
    }

    /// All configured proxy URLs, canonicalised, in rotation order.
    ///
    /// The URLs keep any `user:password@`, since that is what the client
    /// connects with; strip it before you log or print one, or use
    /// [`iter_redacted`](Self::iter_redacted) instead.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator {
        self.proxies.iter().map(String::as_str)
    }

    /// All configured proxies in rotation order as `scheme://host:port/`,
    /// with `***@` before the host when the URL has a user or a password.
    /// The path, query and fragment are left out, and so is a default port.
    /// It is the same form [`Debug`](std::fmt::Debug) output and `tracing`
    /// events use, safe to log or print directly.
    pub fn iter_redacted(&self) -> impl Iterator<Item = String> + '_ {
        self.proxies.iter().map(|p| redact_stored(p))
    }

    /// Index of `proxy`, accepting either the canonical form or anything
    /// that canonicalises to it.
    fn position(&self, proxy: &str) -> Option<usize> {
        self.proxies.iter().position(|p| p == proxy).or_else(|| {
            let canonical = canonical_proxy_url(proxy).ok()?;
            self.proxies.iter().position(|p| *p == canonical)
        })
    }

    /// The proxy at `idx` in the redacted form of
    /// [`iter_redacted`](Self::iter_redacted), for logs and for the proxy
    /// field of an `on_retry` event.
    pub(crate) fn redacted(&self, idx: usize) -> String {
        redact_stored(&self.proxies[idx])
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
    /// their own cooldown expires or [`mark_good`](Self::mark_good) clears
    /// it.
    ///
    /// The URL keeps any `user:password@`, since it is what the client
    /// connects with; strip it before you log or print it.
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

        // One lap: return the first healthy proxy at once, and otherwise
        // track the one that recovers soonest, preferring rotation order
        // on ties, for the fallback below.
        let mut soonest: Option<(usize, Instant)> = None;
        for offset in 0..len {
            let idx = (start + offset) % len;
            match state.bad_until[idx] {
                Some(until) if now < until => {
                    if soonest.is_none_or(|(_, best)| until < best) {
                        soonest = Some((idx, until));
                    }
                }
                _ => {
                    state.bad_until[idx] = None;
                    state.next_index = (idx + 1) % len;
                    return Some(idx);
                }
            }
        }

        // Everything is cooling down: take the one that recovers first.
        let (idx, _) = soonest.expect("len > 0 and every proxy is cooling down");
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

    /// Like [`pick_index`](Self::pick_index), but passes over the proxies
    /// in `avoid` while a healthy one outside it exists.
    pub(crate) fn pick_index_avoiding(&self, avoid: &[usize]) -> Option<usize> {
        if avoid.is_empty() {
            return self.pick_index();
        }
        let len = self.proxies.len();
        {
            let mut state = self.lock();
            let now = Instant::now();
            let start = state.next_index;
            for offset in 0..len {
                let idx = (start + offset) % len;
                if avoid.contains(&idx) {
                    continue;
                }
                if state.bad_until[idx].is_none_or(|until| now >= until) {
                    state.bad_until[idx] = None;
                    state.next_index = (idx + 1) % len;
                    return Some(idx);
                }
            }
        }
        self.pick_index()
    }

    /// Whether a proxy outside `avoid` is out of cooldown right now.
    pub(crate) fn any_healthy_outside(&self, avoid: &[usize]) -> bool {
        let state = self.lock();
        let now = Instant::now();
        state
            .bad_until
            .iter()
            .enumerate()
            .any(|(i, until)| !avoid.contains(&i) && until.is_none_or(|until| now >= until))
    }

    /// Marks a proxy as bad for `cooldown`: [`pick`](Self::pick) will skip
    /// it, unless every proxy is unhealthy, until the cooldown expires, the
    /// proxy answers a request sent through a
    /// [`RotatingClient`](crate::RotatingClient), or
    /// [`mark_good`](Self::mark_good) is called for it, whichever comes
    /// first.
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

    /// Takes a proxy out of cooldown before it expires: your own health
    /// check saw it answer, or you put it there with `mark_bad`.
    ///
    /// `proxy` is matched in canonical form, so both what
    /// [`pick`](Self::pick) returned and what you originally configured
    /// work. Returns `false` if it is not in this list, in which case
    /// nothing changes. A proxy that is not in cooldown is left as it is;
    /// this still returns `true`, since it is in the list either way.
    pub fn mark_good(&self, proxy: &str) -> bool {
        match self.position(proxy) {
            Some(idx) => {
                self.mark_good_index(idx);
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
    #[must_use]
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
        let redacted: Vec<String> = self.proxies.iter().map(|p| redact_stored(p)).collect();
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

/// The one form in which a parsed proxy appears in any text this crate
/// produces: `scheme://`, then `***@` when the URL has a user or a password,
/// the host, the port when it is not the scheme's default, and `/`. The path,
/// query and fragment are never shown: a proxy URL does not use them, and an
/// entry the parser read differently from how it was meant (a password after
/// a stray `/`, `?` or `#`) keeps its secret there.
pub(crate) fn redact_url(url: &reqwest::Url) -> String {
    let userinfo = if url.username().is_empty() && url.password().is_none() {
        ""
    } else {
        "***@"
    };
    let host = url.host_str().unwrap_or_default();
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();

    format!("{}://{userinfo}{host}{port}/", url.scheme())
}

/// [`redact_url`] for a URL kept as text, which `canonical_proxy_url` has
/// parsed once already.
pub(crate) fn redact_stored(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(url) => redact_url(&url),
        Err(_) => redact_unparsable(url),
    }
}

/// Redacts a proxy entry as the caller wrote it, which may not have been
/// checked yet: the redaction of the URL it parses to, the worst-case one of
/// [`redact_unparsable`] for anything else.
pub(crate) fn redact_entry(raw: &str) -> String {
    match canonical_proxy_url(raw) {
        Ok(canonical) => redact_stored(&canonical),
        Err(_) => redact_unparsable(raw),
    }
}

/// Redacts a proxy spelling that `Url::parse` rejected: the authority's
/// end is unknown, so everything up to the last `@` is treated as
/// credentials.
fn redact_unparsable(raw: &str) -> String {
    let raw = without_ignored_characters(raw);
    let Some(at) = raw.rfind('@') else {
        // No `@`, but a password can still be in there: the
        // `host:port:user:pass` export many proxy vendors use. Keep the
        // scheme and host, hide everything after them.
        let (scheme, rest) = split_scheme(&raw);
        let prefix = scheme.map_or_else(String::new, |scheme| format!("{scheme}://"));
        return match rest.split_once(':') {
            Some((host, _)) => format!("{prefix}{}:***", without_path(host)),
            None => format!("{prefix}{}", without_path(rest)),
        };
    };
    // What follows the last `@` is the host and port as far as the entry has
    // them. `host:port@user:pass`, as some vendors write it, puts a password
    // there, so anything after its first `:` that is not a port number is
    // hidden. A path, query or fragment is never shown, as for a parsed entry.
    let tail = &raw[at + 1..];
    let tail = match tail.split_once(':') {
        Some((host, port)) if !port.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{host}:***")
        }
        _ => tail.to_string(),
    };
    let tail = without_path(&tail);
    // A scheme is only what comes before the first `://` when it is a valid
    // one; anything else there, an `@` or a `:` included, is credentials.
    let prefix = split_scheme(&raw[..at])
        .0
        .map_or_else(String::new, |scheme| format!("{scheme}://"));
    format!("{prefix}***@{tail}")
}

/// The text up to the first `/`, `?`, `#` or `\`: the host and port of an
/// entry without its path, query and fragment.
fn without_path(text: &str) -> &str {
    text.find(['/', '?', '#', '\\'])
        .map_or(text, |end| &text[..end])
}

/// Splits `scheme://rest`, but only when what precedes the first `://`
/// is a valid URL scheme (RFC 3986: a letter, then letters, digits, `+`,
/// `-` or `.`). Otherwise the whole text is `rest`: a prefix such as
/// `user:password@host` is userinfo, not a scheme, however many `://` follow.
fn split_scheme(url: &str) -> (Option<&str>, &str) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (None, url);
    };
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if valid {
        (Some(scheme), rest)
    } else {
        (None, url)
    }
}

/// The text as `canonical_proxy_url` and `Url::parse` read it: surrounding
/// whitespace, control characters and spaces dropped, tabs and newlines
/// removed anywhere. A redaction that looked at the raw text would see a
/// different scheme and authority than the parser did.
fn without_ignored_characters(text: &str) -> String {
    text.trim()
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect()
}

/// Whether the entry, as written, holds an `@` after its authority, in a path,
/// query or fragment. Such an entry is credentials the parser read as
/// something else (`user:1234/secret@host` is host `user`, port 1234 and a
/// path), so it never reached the proxy the caller meant. It is decided on
/// the text, not on the parsed URL: normalisation drops the segment that
/// carries the `@` from `user:/secret/@x/../y`. `entry` is the text handed to
/// the parser, so its scheme ends at the first `:`, and `scheme` is the one
/// the parser found there.
fn has_at_after_authority(entry: &str, scheme: &str) -> bool {
    let text = without_ignored_characters(entry);
    let after_scheme = text.split_once(':').map_or("", |(_, rest)| rest);
    // The parser skips the slashes that follow a special scheme and ends the
    // authority at the first of these, a `\` included, so the text is read
    // the same way. Any other scheme has its `//` and nothing more.
    let (rest, delimiters): (&str, &[char]) = if is_special_scheme(scheme) {
        (
            after_scheme.trim_start_matches(['/', '\\']),
            &['/', '?', '#', '\\'],
        )
    } else {
        (
            after_scheme.strip_prefix("//").unwrap_or(after_scheme),
            &['/', '?', '#'],
        )
    };
    let end = rest.find(delimiters);

    end.is_some_and(|end| rest[end..].contains('@'))
}

/// The schemes the URL standard calls special, whose authority ends at a `\`.
fn is_special_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ftp" | "ws" | "wss" | "file")
}

const PERCENT_ENCODE_HINT: &str =
    "not a valid proxy URL; percent-encode `/`, `?` and `#` in the password (`%2F`, `%3F`, `%23`)";
const PERCENT_ENCODE_HINT_SPECIAL: &str = "not a valid proxy URL; percent-encode `/`, `?`, `#` and `\\` in the password (`%2F`, `%3F`, `%23`, `%5C`)";
const SCHEME_HINT: &str = "not a valid proxy URL; a scheme is followed by `://`";

/// Validates a proxy URL and brings it into the canonical form `reqwest`
/// expects: lowercase scheme and host, default port dropped, trailing `/`.
///
/// Accepts `scheme://[user:pass@]host[:port]` for the supported schemes,
/// and a bare `[user:pass@]host[:port]`, which gets `http://` prepended.
/// An `@` after the host, in a path, query or fragment, is refused.
fn canonical_proxy_url(raw: &str) -> Result<String, Error> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::invalid_proxy(
            "(blank entry)",
            "proxy URL is empty; drop blank lines from the list",
        ));
    }

    // An explicit `scheme://` is taken at face value (and its scheme
    // checked below). Anything else (`host:port`, `1.2.3.4:8080`,
    // `user:pass@host:port`) is treated as an HTTP proxy. Trying to parse
    // those directly would misread `host` as a scheme, so do not.
    let has_scheme = raw.contains("://");
    let with_scheme = if has_scheme {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    // Never interpolate `raw` itself into an error message below: it may
    // carry `user:pass@`, and errors must not surface proxy credentials
    // any more than Debug output does. Input the parser rejects goes through
    // `redact_unparsable`, which assumes the worst about where the
    // credentials end; a URL it accepts is shown by `redact_url`.
    let url = reqwest::Url::parse(&with_scheme)
        .ok()
        .filter(reqwest::Url::has_host)
        .ok_or_else(|| {
            Error::invalid_proxy(redact_unparsable(raw), refusal_reason(raw, has_scheme))
        })?;

    if has_at_after_authority(&with_scheme, url.scheme()) {
        // A user the parser already found is a proxy with credentials, and
        // nothing to encode would change which one.
        let reason = if url.username().is_empty() {
            refusal_reason(raw, has_scheme)
        } else {
            "not a valid proxy URL"
        };

        return Err(Error::invalid_proxy(redact_unparsable(raw), reason));
    }

    check_scheme(url.scheme(), &redact_url(&url))?;
    Ok(url.to_string())
}

/// What to tell the caller about an entry that was refused for its spelling:
/// a hint when the entry has a known fix, the bare reason otherwise.
fn refusal_reason(raw: &str, has_scheme: bool) -> &'static str {
    let vendor_spelling = !has_scheme
        && !raw.contains('@')
        && raw.matches(':').count() == 3
        && raw
            .split(':')
            .nth(1)
            .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));

    if vendor_spelling {
        "not a valid proxy URL; write `host:port:user:pass` as `http://user:pass@host:port`"
    } else if !has_scheme && accepted_with_scheme_separator(raw) {
        SCHEME_HINT
    } else if let Some(hint) = delimiter_hint(raw) {
        hint
    } else {
        "not a valid proxy URL"
    }
}

/// Whether the entry, which has no `://`, starts with a scheme and a single
/// `:` and would be accepted with `://` in its place: `http:/user:pw@host`.
fn accepted_with_scheme_separator(raw: &str) -> bool {
    let text = without_ignored_characters(raw);
    let Some((scheme, rest)) = text.split_once(':') else {
        return false;
    };
    // The schemes the crate accepts; any other would be refused for its
    // scheme with or without the `://`.
    let named = matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
    );

    named
        && canonical_proxy_url(&format!(
            "{scheme}://{}",
            rest.trim_start_matches(['/', '\\'])
        ))
        .is_ok()
}

/// The hint for an entry that `canonical_proxy_url` would accept if the `/`,
/// `?` and `#` before its last `@` (and `\` for a special scheme) were
/// percent-encoded, which is the fix the hint names. Testing it by accepting
/// the encoded entry keeps the hint off entries that would still be refused,
/// for a port out of range or an unsupported scheme, say. It calls
/// `canonical_proxy_url` again, one level deep: the encoded entry has none of
/// those characters left before its last `@`.
fn delimiter_hint(raw: &str) -> Option<&'static str> {
    let text = without_ignored_characters(raw);
    let (scheme, rest) = split_scheme(&text);
    let special = scheme.is_none_or(|scheme| is_special_scheme(&scheme.to_ascii_lowercase()));
    let delimiters: &[char] = if special {
        &['/', '?', '#', '\\']
    } else {
        &['/', '?', '#']
    };
    let at = rest.rfind('@')?;
    let (userinfo, host) = rest.split_at(at);
    // Slashes right after the scheme are a typo the parser skips, not part of
    // the password; they stay as they are, so the entry is still refused.
    let password_start = userinfo
        .find(|c: char| !matches!(c, '/' | '\\'))
        .unwrap_or(userinfo.len());
    let (slashes, password) = userinfo.split_at(password_start);
    if !password.contains(delimiters) {
        return None;
    }

    let encoded = password
        .replace('/', "%2F")
        .replace('?', "%3F")
        .replace('#', "%23");
    let encoded = if special {
        encoded.replace('\\', "%5C")
    } else {
        encoded
    };
    let fixed = match scheme {
        Some(scheme) => format!("{scheme}://{slashes}{encoded}{host}"),
        None => format!("{slashes}{encoded}{host}"),
    };

    canonical_proxy_url(&fixed).ok()?;

    Some(if special {
        PERCENT_ENCODE_HINT_SPECIAL
    } else {
        PERCENT_ENCODE_HINT
    })
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
mod tests;
