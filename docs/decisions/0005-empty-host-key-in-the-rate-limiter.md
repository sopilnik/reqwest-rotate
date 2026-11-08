# 0005 A URL without a host uses an empty key in the rate limiter

Status: accepted

## Context

`send_with_retry` asks the per-host rate limiter to wait before each attempt, keyed by the request URL's host:

```rust
inner.rate_limiter.wait(current.url().host_str().unwrap_or(""))
```

`Url::host_str` returns `None` for a URL without a host. The question is whether that case needs its own handling.

## Decision

The `unwrap_or("")` stays, and a URL without a host shares the slot for the empty string.

A URL without a host is never `http` or `https`, and `reqwest` refuses every other scheme in `execute_request`
(`url_bad_scheme`), so a request that reaches the limiter without a host fails before it is sent. The empty slot is
never used by a request that goes out.

It is used by the requests that fail. `get` and `send` (and so a request made with `request`) build the request first,
and `reqwest` refuses a URL without a host at that point, so they never reach the limiter. A `Request` built by hand
without a host and passed to `execute` does: with a rate limit set, it waits its turn on that one slot, `min_interval`
after the previous such request, and `reqwest` then returns its error, a builder error that is not retried. The delay
lands only on a call that was going to fail.

## Alternatives

- Skip the wait when the URL has no host (`if let Some(host) = current.url().host_str()`). Rejected: the only request
  it would speed up fails one step later anyway.
- Give a missing host its own key, with `Option<String>` in place of `String` in the map. Rejected: a lookup would need
  an owned key, which allocates on every request, and the slot would behave exactly like the empty string's.
- Return an error for a missing host before waiting. Rejected: `reqwest` already returns the error, with the right
  type, one step later.

## Consequences

The hot path keeps its borrowed `&str` key: looking up a host that is already in the map does not allocate, and only a
request for a host that is not in the map, its first or its first after the entry was pruned, inserts an owned key. If
`reqwest` ever sent a request without a host, or the crate accepted schemes other than `http` and `https`, this decision
would have to be revisited.
