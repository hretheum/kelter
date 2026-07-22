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

## Coding tools (`[tools.coding]`)

Opt-in. When configured, the model gets allowlisted command execution
(`uruchom_komende`, default `cargo`+`git`) inside the configured `root` with
the daemon's full OS authority. Reads, writes and command `cwd` are confined
to `root` (lexical `..`/absolute rejection plus symlink containment), but a
command itself can do anything the daemon's user can. Point `root` only at a
disposable checkout or fixture; keep real repositories behind an explicit
human decision.

Two standing, deliberate boundaries elsewhere in the tool surface:

- The **reference** (report) tools' `read_source` deliberately accepts
  absolute paths — reads are unrestricted by design (a covering test pins
  this); only writes are workspace-confined. The coding tools do NOT inherit
  this: their reads are root-confined.
- An extension manifest's `network = false` is **recorded and logged, not
  enforced** at the transport level (see `crates/mcp/src/extensions.rs`).
  Subprocess extensions have OS-level egress; isolation must come from
  outside (container, dedicated worktree, `sandbox-exec`). WASM extensions
  are the sandboxed alternative (ADR 0011/0024).
