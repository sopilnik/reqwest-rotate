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

## Alternatives

- Key the limiter by `Option<&str>` and skip the wait for `None`. Rejected: the map would need an owned `Option<String>`
  key, which allocates on every request, to handle a case that cannot reach the network.
- Return an error for a missing host before waiting. Rejected: `reqwest` already returns the error, with the right
  type, one step later.

## Consequences

The hot path keeps its borrowed `&str` key and does not allocate. If `reqwest` ever sent a request without a host, or
the crate accepted schemes other than `http` and `https`, this decision would have to be revisited.
