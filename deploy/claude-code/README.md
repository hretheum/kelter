---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-5-5
  session_date: 2026-10-13
  human_validated: false
  session_context: "Roadmap KRW.1 (stage A of the cost path): Claude Code as a headless executor under Kelter, built from extensions and a skill with no kernel change. Eryk's decisions 2026-10-13: Claude Code runs on the Max subscription; Opus always asks. Security model revised after an independent review the same day (H1–H3, M1–M5)."
---

# Claude Code under Kelter (cost path, KRW.1)

Kelter decides *how* a coding task is done — no Claude Code, Haiku, Sonnet or
Opus — prepares the brief, has Claude Code do the work on a branch, checks
the result with a real command, and escalates only after a failed check.
Nothing here changes the kernel: it is a runner service, a Kelter extension
and an advisory skill, all deployed on K12.

```
Kelter (container, llm-net)                   claude-code-runner (container, llm-net)
  model + skill "sciezka-kosztowa"              POST /v1/run   → fresh clone, `claude -p` as `worker`
  extension "claude-code" ── stdio MCP ──►      POST /v1/check → a named check as `worker`
    claude-code-bridge  ──── HTTP + bearer ──►  /repos (read-only) · /work (per task) · /out (bundles)
```

| piece | where | what |
|---|---|---|
| runner | `runner/` | Node 22, no dependencies. One task at a time. Exactly once per `idempotency_key`: a repeat returns the stored result, a task cut off by a restart is reported failed, never re-run. Each task: a fresh clone on branch `kelter/<12 hex>`, its own HOME and caches; leftovers committed; the result leaves as a git bundle. Never pushes. |
| bridge | `bridge/` → `../extensions/claude-code/` | static Rust binary, stdio MCP subset (`initialize`, `tools/list`, `tools/call`, `ping`). Tools come from `bridge.json`. Runner `ok: false` / HTTP error → `isError` → a failed step. |
| skill | `skill/sciezka-kosztowa/` | the rules: when to skip Claude Code, which model first, brief, check, escalation. Advisory only. |
| unit | `claude-code-runner.container` | rootless Quadlet, `Network=llm-net`, no published port, `--init`, `Memory=12G`. |
| fetch | `pobierz.sh` | on the Mac: copy a result bundle from K12 and fetch its branch. |

## Governance — what holds, and where it stops

- **Opus always asks (at Kelter's boundary).** `claude_code_opus` is its own
  tool with `model` fixed by the bridge (overwritten even if the model sends
  one); `claude_code_run`'s schema allows only `haiku`/`sonnet` and the kernel
  validates it. With `[approval.actions] claude_code_opus = "ask"` every Opus
  call pauses the run until a human decides (ADR 0038); actions have no
  "allow always".
- **Opus inside a Sonnet/Haiku session.** A session holds the subscription
  token, so it could technically start Opus itself. Accidental paths are
  closed: subagents and the `opus` alias are pinned to the run's own model
  (`CLAUDE_CODE_SUBAGENT_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`), and a nested
  `claude` through Bash is denied (`--disallowedTools`). Opus in the session's
  own `modelUsage` on a non-Opus run fails the step (`naruszenie: … użyła
  Opusa bez zgody`) — it cannot count as success. Residual risk, stated
  plainly: a session that deliberately works around this (a nested session
  started some other way, or the API called directly with its token) is
  neither prevented nor reliably detected. Nothing of a session outlives it
  (every worker process is killed after each task), so the token is exposed
  only for the task's own duration.
- **Mutating, exactly once.** Both run tools declare `idempotency_key`; the
  kernel keys them `run:step:attempt` (ADR 0031) and the gate sees them; the
  runner dedupes on that key.
- **Proof, not claims.** `sprawdz` runs only for a task that finished cleanly
  *and* changed something (tests passing on an untouched base prove
  nothing). Use a contract on both:
  `{"require": [{"kind": "action_succeeded", "action": "claude_code_run"},
  {"kind": "action_succeeded", "action": "sprawdz"}]}` (for an Opus path
  name `claude_code_opus`). Without a contract the run ends unverified and is
  not learned from (ADR 0037). A check runs the branch's own test code: a
  session that weakens a test weakens the evidence — review the diff.
- **Bounded.** Per task: `limit_czasu_s` (wall clock; the runner kills the
  process group) and `--max-budget-usd` per model. Per run: `[budget]`
  `max_wall_secs` (ADR 0039; paused time does not count). Kelter's dollar
  budget does not see Claude Code's cost yet — that is KRW.3.

## Isolation

- **Two identities.** The runner is the container's root — rootless podman,
  i.e. the host user, nothing more — and alone reads the mounted secrets.
  Claude and every check run as `worker` (uid 1001) with an explicit
  environment: no runner token ever; the subscription token only for a
  claude session, never for a check (checks run code the model wrote —
  `build.rs`, lifecycle scripts).
- **No shared writable state between tasks.** Each task gets its own HOME,
  `TMPDIR`, `CARGO_HOME`, `CARGO_TARGET_DIR` and pnpm store under
  `/work/<id>`; the toolchain (`/usr/local/{rustup,cargo}`) and the baked pnpm
  versions (`/opt/corepack`) are not writable by the worker; and after every
  worker command the runner kills every process of the worker uid (one task
  runs at a time), so nothing — not even a `setsid` escapee — survives into
  the next task. Cost: each task builds from cold (the first `kelter-testy`
  of a task compiles the whole workspace; measure on K12, and raise
  `timeout_s` if needed). Caches of all but the newest `keep_caches` tasks
  are deleted; clones and bundles are not.
- **Nothing the host runs is model-writable.** `/repos` is mounted
  read-only: a session cannot add a hook or move `main` in the repos the Mac
  pushes to. Results leave as a bundle file in `/out` (written by the
  runner, refusing anything but a regular file); the Mac fetches the bundle,
  never a repo the session touched.

Not covered: individual commands inside a session (the container is the
sandbox: no push credentials, no host access beyond its volumes). Per-command
control is KR5.6.

## Install on K12

As the user that runs the `kelter` unit, from a checkout of this repo.

**1. Secrets** (never in the repo or the unit):

```sh
mkdir -p ~/.config/claude-code-runner && chmod 700 ~/.config/claude-code-runner
openssl rand -hex 32 > ~/.config/claude-code-runner/runner-token
chmod 600 ~/.config/claude-code-runner/runner-token
cp deploy/claude-code/runner/config.example.json ~/.config/claude-code-runner/config.json
```

The Claude Code subscription token: on the Mac run `claude setup-token`
(browser login, prints a long-lived token), then put it on K12 without it
passing through any log:

```sh
ssh k12 'umask 077; cat > ~/.config/claude-code-runner/claude-oauth-token'   # paste, Enter, Ctrl-D
```

**2. Repositories** — bare repos on the host, read-only for the runner. The
Mac pushes the base branch in; results come back as bundles:

```sh
# on K12
mkdir -p ~/cc-runner/{repos,work,state,out}
# --shared=0644: objects stay world-readable even under a strict umask —
# the worker (uid 1001 in the container) reads them only through "other" bits.
for r in kelter aids glassbox; do git init -q --bare --shared=0644 -b main ~/cc-runner/repos/$r.git; done

# on the Mac, once per repo, then before tasks that need a fresh base
git remote add k12 k12:cc-runner/repos/kelter.git
git push k12 main
```

**3. Runner**

```sh
podman build -t localhost/claude-code-runner:latest deploy/claude-code/runner
cp deploy/claude-code/claude-code-runner.container ~/.config/containers/systemd/
systemctl --user daemon-reload && systemctl --user start claude-code-runner
podman exec claude-code-runner claude --version          # the pinned CLI
```

**4. Extension** — inside the directory `deploy/kelter.container` already
mounts at `/data/config`:

```sh
deploy/claude-code/build-bridge.sh
mkdir -p ~/.config/kelter/extensions/claude-code
cp deploy/extensions/claude-code/{kelter-extension.toml,bridge.json,claude-code-bridge} \
   ~/.config/kelter/extensions/claude-code/
install -m 600 ~/.config/claude-code-runner/runner-token ~/.config/kelter/extensions/claude-code/
```

**5. Kelter config** (`~/.config/kelter/config.toml`):

```toml
[tools]
extension_dirs = ["/data/config/extensions"]

[trust]
allow_unsandboxed_subprocess = ["claude-code"]

[approval]
ask_timeout_secs = 86400          # an unanswered Opus request is denied after a day
[approval.actions]
claude_code_opus = "ask"

[skills]
active = ["sciezka-kosztowa"]

[budget]
max_wall_secs = 14400             # a whole run, escalations included
```

**6. Skill**

```sh
mkdir -p ~/.config/kelter/skills-src && cp -r deploy/claude-code/skill/sciezka-kosztowa ~/.config/kelter/skills-src/
podman exec kelter kelter --config /data/config/config.toml skill add /data/config/skills-src/sciezka-kosztowa
systemctl --user restart kelter
curl -s -H "Authorization: Bearer $KELTER_TOKEN" http://127.0.0.1:7466/api/v1/tools   # claude_code_run, claude_code_opus, sprawdz
```

Housekeeping: files under `~/cc-runner/work` belong to the worker's subuid
on the host; remove old tasks with `podman unshare rm -rf ~/cc-runner/work/<id>`.

## Using it

```sh
curl -s -X POST -H "Authorization: Bearer $KELTER_TOKEN" -H 'Content-Type: application/json' \
  http://127.0.0.1:7466/api/v1/runs -d '{
    "task": "W repo kelter: popraw ostrzeżenie clippy w crates/mcp. Sukces: kelter-clippy przechodzi.",
    "success_contract": {"require": [
      {"kind": "action_succeeded", "action": "claude_code_run"},
      {"kind": "action_succeeded", "action": "sprawdz"}]}
  }'
```

An Opus request shows up in `GET /api/v1/approvals` (and the web UI);
decide with `POST /api/v1/runs/{id}/action-approval`
`{"action": "claude_code_opus", "decision": "allow"|"deny"}`.

The result: `deploy/claude-code/pobierz.sh <zadanie> ~/dev/prv/products/kelter <sha256>`
fetches branch `kelter/<zadanie>` into the local repo for review.

## Tests

- `node --test deploy/claude-code/runner/test/*.test.mjs` — runner (real git, fake `claude`).
- `cargo test --manifest-path deploy/claude-code/bridge/Cargo.toml` — bridge.
- `scripts/smoke-claude-code.sh` — the real daemon end to end: Sonnet run
  verified by contract, result as a bundle, base repo untouched; a failed
  check → `incomplete`; Opus pauses for a human, then runs once.
