---
provenance: ai_authored
provenance_metadata:
  agent: claude-sonnet-5-5
  session_date: 2026-10-29
  human_validated: false
  session_context: "Storyboard rewritten from the 2026-07-18 version (roadmap KR3.4): three proofs from the README, two recording variants. Commands for variant (b) are taken from the repo's own scripts; what was and was not run while writing this is stated in 'What was verified'. Recording is a human act."
---

# Demo recording storyboard

Target: one hero GIF (≤ 30 s, loops cleanly) + supporting clips, built from
the three proofs the README makes (and a bonus for the human gate). The
README lists four proofs; the demo uses the three that show best on screen.

| # | Proof | README claim | Reproducible with |
|---|---|---|---|
| 1 | Same task on a 0.5B local model and a frontier model, side by side | proof 1 | `scripts/demo-side-by-side.sh` |
| 2 | Bad action proposed → kernel rejects it → model retries | "Kernel validation on top, always" | `scripts/smoke-journal-history.sh` leg R (scripted model) |
| 3 | Kill mid-run → resume without asking the model → side effect exactly once | proof 2 | `scripts/smoke-durability.sh` leg B, or `smoke-journal-history.sh` leg K |
| bonus | `ask` gate: pause before a mutating action, survives restart, `allow` runs it once | proof 3 | `scripts/smoke-human-gate-budgets.sh` leg A |

## Two ways to record

**(a) On Glassbox** (the graph viewer, fork `hretheum/glassbox`, **private**,
license undecided). Graph instead of logs: rejections and `incomplete` show as
"kernel interventions" on amber (per roadmap KR2.4; the roadmap's early idea
was red), history reads ADR 0036 journal entries so a finished run shows the
same as a live one. Open items from the roadmap: the `incomplete` agent card
covers the first call; the demo run should avoid that case or wait for the fix.
Recording the UI shows no source, but a gallery asset from a private
fork points at something viewers cannot open, and "joint launch / cross-link
with Glassbox" needs the fork public with a license. **Decision for Eryk:**
publish Glassbox first, or use (b) for launch and add (a) later.

**(b) Without Glassbox**: terminal (asciinema or VHS) and, where noted,
Kelter's own web UI. This is what can be recorded today from this repo alone.
What it does not give: a graph. Proof 1 and the journal are tables and JSON.
The web UI does show a rejection on the step card
("Kernel rejected this step: <reason> · retry <n>",
`apps/web-ui/src/features/runs/StepCard.tsx`) and has an approval prompt for
`ask`, but neither was clicked through for this document.

Glassbox variant, per proof: start the same daemons as in (b), point
Glassbox at the daemon URL and token, open the run; take proof 1 as two
Glassbox windows side by side (one per daemon: ports 7521 and 7522 in
`demo-side-by-side.sh`), proof 2 on the amber node and its retry, proof 3 on
the run history after the restart (one `resumed` marker, replayed step). Glassbox
steps are not reproduced here because its repo is not readable from this one.

## Prep for (b), once

```sh
# tools: asciinema + agg (GIF) or VHS; none are installed by the repo
brew install asciinema agg      # or: brew install vhs
# terminal: 120×32, font ≥ 16 pt, dark theme, short prompt (PS1='$ ')
git clone https://github.com/hretheum/kelter && cd kelter
pnpm --dir apps/web-ui install --frozen-lockfile && pnpm --dir apps/web-ui build
                                # the api crate embeds apps/web-ui/dist; a fresh clone
                                # does not compile without it (checked 2026-10-29)
cargo build --workspace         # warm the cache (several GB of target/)
llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096 &   # proofs 1 and 3b
export OPENROUTER_API_KEY=...   # from a keychain, never on screen; proof 1 frontier leg
```

Take the prompt through `history -c` and do not show shell history or the
environment.

## Proof 1 — one line of config, two engines (hero GIF, ≈ 22 s)

Commands to repeat, in order:

```sh
diff <(grep '^engine' examples/config-local.toml) \
     <(grep '^engine' examples/config-openrouter.toml)   # the single changed line
scripts/demo-side-by-side.sh                              # builds, runs local then frontier
```

| t | on screen | action |
|---|---|---|
| 0–3 s | title card: "Same agent. Same task. Two engines." | pre-rendered |
| 3–6 s | the `diff` output | run |
| 6–20 s | `demo-side-by-side.sh`: step tables for local, then frontier, both `completed` | run, 2× speed in the edit |
| 20–22 s | freeze on the two summary lines + caption "identical step sequence" | hold |

Notes from the script: it needs `llama-server` on :8080 and skips the frontier
leg without `OPENROUTER_API_KEY`; it prints `── local … → completed in Ns`.
Timing on an M-series MacBook (2026-07 draft): local ≈ 2 s, frontier ≈ 12 s.
Cut the `building…` line in the edit. Evidence from an earlier run:
`docs/evidence/demo-local-run.json`, `docs/evidence/demo-frontier-run.json`,
`docs/evidence/demo-side-by-side.log`.

## Proof 2 — the kernel rejects a bad action, the run goes on (clip, ≈ 15 s)

Determinism matters more than realism here, so this proof uses the scripted
model stub (`scripts/fake-llm-scripted.py`): its first reply is a
schema-invalid action (`entry: 5`), the second a valid one, the third a
duplicate, then `finish`. Say so in the caption ("scripted model: the point is
the kernel").

Fast version, one command (about 20 s plus the first build):

```sh
scripts/smoke-journal-history.sh /tmp/kelter-evidence
jq -r '.journal[] | select(.kind=="rejected") | [.step,.source,.subject,(.arguments|tojson)] | @tsv' \
   /tmp/kelter-evidence/journal-history-rejections.json
```

Expected from the committed copy in the repo (`docs/evidence/adr-0036/`):
two `rejected` entries, `(step 0, schema, proposal, {"entry":5})` and
`(step 1, duplicate, proposal, {"entry":"journal-hello"})`, and the script's
line `R: PASS — 2 refusals in the history, each with the refused proposal`.

UI version (shows the "Kernel rejected this step" card): start the stub and
a daemon by hand exactly as the script does (config in
`scripts/smoke-journal-history.sh`, lines 36–64: `[policy] engine =
"local:scripted-stub"`, `base_url = "http://127.0.0.1:18091"`), open
`http://127.0.0.1:<port>/`, paste the token, submit
`Append journal-hello to the ledger, then finish.`

| t | on screen | action |
|---|---|---|
| 0–3 s | caption "a weak model proposes a malformed action" | card |
| 3–10 s | run starts; `rejected` (schema) then a valid step, then `rejected` (duplicate), then `completed` | UI, or the `jq` output |
| 10–15 s | freeze on the rejected entry showing the refused proposal | hold |

## Proof 3 — kill it mid-run, watch it finish (clip, ≈ 20 s)

Worst crash window: killed after the side effect fired, before its receipt.
Uses the `KELTER_CRASH_POINT` hook (`crates/kernel-core/src/engine.rs`).

Option 1, real 0.5B model (needs `llama-server` on :8080, `sqlite3`):

```sh
scripts/smoke-durability.sh /tmp/kelter-evidence
# watch leg B: "daemon crashed after the append; ledger has 1 line, no receipt journaled"
#               "B: PASS — resumed to completion, side effect exactly once"
grep -E "model NOT queried|journaled idempotency key" /tmp/kelter-evidence/durability-resume-after-effect.log
```

Option 2, scripted model, no llama.cpp: the same crash and resume is leg K of
`scripts/smoke-journal-history.sh` (output line `K: PASS — one resume entry,
session of the restarted process, step 0 replayed, effect once`), then

```sh
jq -r '.journal[] | select(.kind=="resumed")' /tmp/kelter-evidence/journal-history-resume.json
```

| t | on screen | action |
|---|---|---|
| 0–4 s | run starts; ledger gets one line; process dies (`KELTER_CRASH_POINT hit`) | script output |
| 4–8 s | restart; log: `[resume] replaying journaled decision — model NOT queried` | script output |
| 8–16 s | `[resume] dangling intent — re-dispatching with the journaled idempotency key` → `completed` | script output |
| 16–20 s | `wc -l work/ledger.txt` shows **1**; caption "exactly once" | hold |

## Bonus — ask before it costs (clip, ≈ 20 s, optional)

```sh
scripts/smoke-human-gate-budgets.sh /tmp/kelter-evidence
```

Leg A pauses at `awaiting_action_approval`, restarts the daemon, the run is
re-surfaced without the model being called and the ledger untouched, then an
`allow` appends the line once. The captured JSON (state, queue entry, journal
order `action_approval_requested → action_approval_decided → resumed →
decision → intent → receipt`) is in BUILD-EVIDENCE, entry "2026-10-05 — human
gate on mutating actions + cost budgets", and `docs/evidence/2026-10-05-human-gate-budgets/`.

## Recording with VHS or asciinema

```sh
# asciinema: record, then render a GIF
asciinema rec proof1.cast --cols 120 --rows 32 -c 'scripts/demo-side-by-side.sh'
agg --speed 2 proof1.cast proof1.gif

# VHS: a tape drives the keystrokes, so takes are repeatable
cat > proof1.tape <<'TAPE'
Output proof1.gif
Set Width 1400
Set Height 700
Set FontSize 18
Set Theme "Dracula"
Type "scripts/demo-side-by-side.sh" Enter
Sleep 25s
TAPE
vhs proof1.tape
```

## Gallery shot list

1. Hero GIF — proof 1.
2. Proof 2 clip, ending on the rejected entry with the refused proposal.
3. Proof 3 clip, ending on the one-line ledger.
4. Static: the `awaiting_action_approval` JSON / UI prompt (bonus).
5. Static: README architecture diagram, styled; topology card.

## What was verified while writing this

- Script and file references above were read in the repo at `9240cb8`.
- `scripts/smoke-journal-history.sh` was run end to end on 2026-10-29 for this
  document (legs R and K both PASS, after building the web UI first), and the
  two `jq` commands above were run on the files it wrote and print exactly the
  two rejections and the one `resumed` entry described. Nothing else was
  run: proof 1 needs llama.cpp and an OpenRouter key, proof 3 option 1 needs
  llama.cpp, the web UI was not driven in a browser, and neither asciinema
  nor VHS was installed, so no recording was made.
- Timings are from the 2026-07-18 draft, not re-measured.

## Recording notes

- Both engines run at temperature 0; step sequences are reproducible. The
  frontier leg's wall time varies a few seconds with API latency.
- If a take glitches, re-run the script: daemons are per-run, workspaces are
  temp dirs.
- No scene prints the API key. Do not show shell history or `env`.
