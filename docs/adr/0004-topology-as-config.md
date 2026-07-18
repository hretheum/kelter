# ADR 0004: One kernel, topology as config, thin-by-default clients

Status: accepted · Date: 2026-07-18

## Context

"Server", "standalone desktop", "remote client" and "mobile" are usually
built as separate applications, multiplying state handling and sync bugs.

## Decision

There is exactly **one kernel** (`kernel-core`, a library crate) and **one
API contract** (`api`: HTTP for commands, WebSocket for event streaming,
bearer-token auth). Topology is decided by configuration, not by code:

- **Server/container**: the `daemon` binary embeds kernel + API + web UI.
- **Standalone desktop**: the desktop shell links the same crates and runs
  the daemon *in-process*, pointing its webview at localhost.
- **Remote client**: the same shell (or any browser/PWA on the tailnet)
  points at a remote daemon URL — no kernel code on the client at all.

The kernel owns all state; clients are stateless views over the API. The
policy engine choice is likewise one config line (`[policy] engine = ...`).

## Consequences

- One codebase, three topologies; a fix lands everywhere at once.
- The web UI is written once and reused by the daemon (PWA) and the desktop
  shell (webview).
- Auth is the same bearer token everywhere; network-level trust (e.g. a
  tailnet) is composable on top, and the API is never meant to be exposed
  publicly without hardening.
- Multi-master offline/online sync (CRDTs) is explicitly out of scope for
  v1; the single-kernel model avoids needing it.
