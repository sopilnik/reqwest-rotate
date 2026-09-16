# Security policy

## Supported versions

Only the latest version gets security fixes. I don't backport fixes to older versions.

## Reporting a vulnerability

Please don't open a public issue for a security problem. Report it privately instead: go to the Security tab of this repository and click the "Report a vulnerability" button.

Proxy URLs often contain a username and password. They must never show up in `Debug` output, error messages or tracing events, so if you find a case where they do, please report it as a vulnerability.

I'll try to reply within a week. If I can confirm the problem, I'll release a fix and publish a GitHub security advisory. The advisory will credit you, unless you'd rather not be named.
