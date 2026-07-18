---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 2: recording storyboard for the launch demo (recorded by a human at the gate; not by the agent)"
---

# Demo recording storyboard

Target: one hero GIF (≤ 30 s, loops cleanly) + 2 supporting clips. Every
scene below was executed on this machine during Phase 2 — commands are
copy-paste ready and deterministic (temperature 0 on both engines).

## Prep (once, before recording)

```sh
# terminal: 120×32, big font (≥16 pt), dark theme, shell prompt shortened
llama-server -hf Qwen/Qwen2.5-0.5B-Instruct-GGUF:q4_k_m --port 8080 -c 4096 &
export OPENROUTER_API_KEY=...        # from your keychain, NOT committed
cargo build --workspace              # warm the build cache
./scripts/demo-side-by-side.sh       # dry run — confirm both legs complete
./scripts/smoke-extension.sh         # dry run — confirm extension leg
```

Timing observed on an M-series MacBook: local leg ≈ 2 s, frontier leg
≈ 12 s, extension smoke ≈ 15 s warm (first run adds venv creation).

## Scene 1 — the one-line swap (hero GIF, ≈ 22 s)

| t | on screen | action |
|---|---|---|
| 0–3 s | title card: "Same agent. Same task. Two engines." | pre-rendered card |
| 3–6 s | `diff examples/config-local.toml examples/config-openrouter.toml` output — single `engine =` line highlighted | run the diff |
| 6–20 s | `./scripts/demo-side-by-side.sh` — step tables print for **local** then **frontier**, both ending `completed` | run it; speed up 2× in edit if needed |
| 20–22 s | freeze on the two summary lines (`completed in 2s` / `completed in 12s`) + caption "the engine is one config line" | hold |

Capture: script prints everything; no manual typing mid-scene. Cut the
`building…` line in edit.

## Scene 2 — extend without Rust (clip, ≈ 20 s)

| t | on screen | action |
|---|---|---|
| 0–4 s | `cat extensions/wordstats/server.py` — the whole file fits one screen; caption "a new agent tool: ~25 lines of Python" | run |
| 4–7 s | `cat extensions/wordstats/kelter-extension.toml` — caption "declare it" | run |
| 7–15 s | `./scripts/smoke-extension.sh` — shows `registered: word_stats, finish`, then the model calling `word_stats` and the structured result | run |
| 15–20 s | freeze on `PASS: Python extension registered via manifest and used by the model — no kernel recompile` | hold |

## Scene 3 — live UI (clip, ≈ 10 s, optional for gallery)

| t | on screen | action |
|---|---|---|
| 0–2 s | browser at the daemon UI (run `scripts/bootstrap.sh` first; it opens the UI with the token) | — |
| 2–10 s | paste the demo task into the Task box, press Run, events stream live (proposed → executed → … → completed) in the event log | submit |

Demo task (paste verbatim, replace `<repo>`):

```
Read the file <repo>/examples/sample.md and save it as raw.md. Then extract the structure of raw.md into structure.json. Then write a markdown report from structure.json to report.md. Then finish.
```

## Shot checklist for the gallery

1. Hero GIF (Scene 1).
2. Scene 2 as GIF or 3-frame sequence (code → manifest → PASS).
3. Static: README architecture diagram, styled.
4. Static: the event-log close-up with a `rejected` line followed by a
   successful retry (grab from any local-leg run at higher latitude — shows
   the kernel enforcing the schema).
5. Static: topology card (container / desktop / PWA-over-tailnet).

## Recording notes

- Both engines run at temperature 0; step sequences are reproducible. The
  frontier leg's wall time varies a few seconds with API latency.
- If a take glitches, just re-run the script — daemons are per-run and
  self-cleaning; workspaces are temp dirs.
- Nothing in these scenes prints the API key; the scripts mask nothing
  because they log nothing secret. Don't show your shell history.
