# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately. Do not open a public issue
for a suspected vulnerability.

Contact: security contact to be published before the first public release;
until then use GitHub private vulnerability reporting once the repository is
public.

Please include as much detail as possible: affected component, reproduction
steps, and potential impact. We will acknowledge reports and work with you on
a fix and disclosure timeline.

## Supported versions

Only the pre-release `main` branch is currently supported. There are no
tagged releases yet, and no older versions receive security fixes.

## Deployment note

The API server uses bearer-token authentication and is designed to sit behind
a private network (for example, a tailnet). It is not designed to be exposed
publicly without additional hardening (TLS termination, network-level access
control, rate limiting, etc.). Do not expose it directly to the public
internet without such measures in place.
