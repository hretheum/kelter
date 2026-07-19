---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 11: security-surface hardening — an auth-surface audit, an SSRF fix on the 8.5 config health-check (bearer + origin allowlist), WS-auth hardening, per-caller principals with per-run attribution, and opt-in rustls TLS. The default local/tailnet bearer flow is unchanged; everything else is additive."
---

# ADR 0018: Security-surface hardening

Status: accepted · Date: 2026-07-19 · Builds on: Phase 1–3 (bearer API), ADR 0009 (origin-pin/SSRF pattern — reused), Phase 8/F (AuthLimiter), ADR 0013/0012 (per-run attribution — extended with the principal), ADR 0015 (the health-check being fixed)

## Context

An audit of the HTTP/WS/config surface found the exposure closed except for a
few gaps. The **default local/tailnet flow (a single bearer token + rate
limiter) must stay simple and unchanged** — everything here is additive/opt-in.

### Auth-surface audit

| Route | Auth (before) | After |
|---|---|---|
| `GET /healthz` | public | public (health probe — intentional) |
| `GET /.well-known/agent-card.json` | public | public (A2A discovery; declares the scheme) |
| static UI (`fallback`) | public | public (serves the web app) |
| `/api/v1/runs*`, `/tools`, `/config/model{,/key}`, `/learn*`, `/a2a/rpc` | bearer + rate-limit | unchanged |
| `POST /api/v1/config/model/test` | bearer, **arbitrary target** | bearer **+ origin allowlist** (SSRF fix) |
| `GET /api/v1/events` (WS) | token (header/query), **no rate-limit** | full `auth_gate` (token + rate-limit) |
| per-run attribution | single "authenticated" identity | **named principal** per run |

## Decisions

### B. SSRF fix on the config health-check (crux #2)

"Test connection" (ADR 0015) fetched an arbitrary caller-supplied `base_url` —
a bearer-authenticated SSRF (an authenticated caller, or a token-bearing XSS'd
browser, could make the daemon probe a cloud-metadata IP or internal host). Fix,
reusing the ADR 0009 origin-pin pattern: the probe target's **origin** must be
on an allowlist — the fixed presets (`http://127.0.0.1:8080`,
`http://127.0.0.1:11434`, `https://openrouter.ai`), the currently-configured
`base_url` origin, or an operator-set `[auth] test_allowlist`. Off-allowlist →
refused, never fetched; redirects are disabled so an allowlisted origin can't
bounce elsewhere. It stays bearer-gated. A Custom endpoint is validated at the
real run; to pre-probe it, the operator adds its origin to `test_allowlist`.

### C. WS-auth

The WebSocket events channel already required a token (header or query), but
bypassed the rate limiter. It now goes through the **same `auth_gate`** as REST
(rate-limit + token, header or query — browsers can't set WS headers). No token
or a wrong token → the handshake is refused before upgrade.

### D. Per-caller principals (crux #1)

A single anonymous "authenticated" identity became **named principals** without
breaking the single-token flow. `Principals` maps each token to a principal
name; `[auth] principals` names each principal + the **env var** holding its
token (the token value never touches the config tree). The default
`[server] token` is principal **"default"** — so a deployment that configures no
principals behaves exactly as before. The authenticated principal is
**attributed per run** in the journal (a `principal` Note on API-created runs;
the `a2a_inbound` Note gains a `principal` field), extending the existing
attribution seam. Tokens are compared in constant time across all principals.

### E. TLS termination (opt-in)

`[tls] cert`/`key` (PEM, gitignored) makes the daemon serve **HTTPS** via
`axum-server` + rustls (aws-lc-rs provider pinned to avoid an ambiguous default,
since reqwest also pulls ring). Absent → plain HTTP (the default; a tailnet
provides transport security). The same `axum::Router` serves both. mTLS and
OAuth/OIDC are **deferred**. A self-signed cert suffices for a demo; a real
cert is a human deployment step.

## Consequences

- The health-check can no longer be turned into an SSRF probe; the WS channel is
  fully gated; runs are attributable to a named caller; and the daemon can serve
  HTTPS when exposed beyond a tailnet — all while the default single-token
  local/tailnet flow is byte-for-byte unchanged.
- Secrets stay out of the tree and logs: principal tokens come from env, the API
  key from env/a 0600 file (ADR 0015), TLS cert/key are gitignored PEM, and the
  daemon logs only a masked token fingerprint (ADR 0017). Proven by
  `scripts/smoke-security.sh` + the api/daemon tests.
- Deferred, explicit: mTLS (client-cert), OAuth/OIDC, and OS-level subprocess
  containment (WASM, ADR 0011, covers that need). Per-caller *authorization*
  (principals with different permissions) is out of scope — principals here are
  identity/attribution, not RBAC.
