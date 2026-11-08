# Security policy

## Supported versions

Only the latest version gets security fixes. I don't backport fixes to older versions.

CI checks the dependencies against the [RustSec advisory database](https://rustsec.org/) on every push to `main` and once a week. Pull requests skip the advisory check, so a new advisory never blocks an unrelated change; the license, banned-crate and source checks still run on them. If an advisory means this crate has to require a newer version of a dependency, I release that as a patch version.

## Reporting a vulnerability

Please don't open a public issue for a security problem. Report it privately instead: [open a private vulnerability report](https://github.com/sopilnik/reqwest-rotate/security/advisories/new), or go to the Security tab of this repository and click the "Report a vulnerability" button.

Proxy URLs often contain a username and password. They must never show up in `Debug` output, error messages, tracing events or the `proxy` field of an `on_retry` event, so if you find a case where they do, please report it as a vulnerability.

I'll try to reply within a week. If I can confirm the problem, I'll release a fix and publish a GitHub security advisory. The advisory will credit you, unless you'd rather not be named.
