---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 2: launch-grade Product Hunt asset draft — final voice pass, scheduling and submission are human acts at the gate"
---

# Product Hunt launch — draft (human finalizes & submits)

> Status: launch-grade **draft**. The maker does the final voice pass,
> records the demo (see [demo-storyboard.md](demo-storyboard.md)), secures
> the name/handles, schedules and submits. Nothing here is auto-published.

## Name & tagline

**Kelter** — pick one:

1. **Agent orchestration that survives weak models.** *(working favorite)*
2. The kernel is the runtime. The model is just the policy.
3. Swap a frontier model for a 0.5B local one — same agent, one config line.
4. A deployable agent runtime: deterministic kernel, swappable models,
   extend it in Python.
5. Stop letting tool-call luck run your agents.

Gallery hook (under the hero GIF): *"Same task, same kernel — 0.5B local
model on the left, frontier model on the right, one config line apart."*

## Maker comment (draft — needs personal voice pass)

> Every agent framework I tried had the same failure mode: the moment the
> model fumbled a tool call, the whole workflow fell over. The model *was*
> the runtime.
>
> Kelter inverts that. A deterministic Rust kernel owns control flow —
> retries, validation, journaling — and the LLM is a constrained policy
> oracle. Locally, a GBNF grammar makes malformed tool calls literally
> unrepresentable at the decoder; remotely, native tool-calling does the
> same job; the kernel re-validates everything either way and deterministic
> anti-stall rules keep even a 0.5B model from looping.
>
> Two things I'd love you to poke at:
>
> 1. **The one-line swap.** The demo task runs identically on a local
>    Qwen-0.5B and a frontier model — the diff between the two configs is a
>    single `engine =` line.
> 2. **Extend it without Rust.** A new agent tool is ~25 lines of Python
>    (official MCP SDK) plus a 10-line manifest — no recompilation, and it
>    still goes through kernel-side schema validation like everything else.
>
> It's OSS (MIT/Apache-2.0), Rust, MCP-native, one kernel for server /
> desktop / PWA. Brutal feedback on the adapter contract and the extension
> model very welcome.

## First-comment FAQ (prepared answers)

- **Why not just use <framework>?** We don't position against names — the
  difference is architectural: here the model never executes anything and
  the runtime never trusts the model. If your orchestration survives
  swapping GPT for a 0.5B local model, that property came from somewhere;
  here it comes from the kernel.
- **Isn't a constrained 0.5B model still dumb?** Yes — it makes *worse
  decisions*, slower progress, simpler plans. What it can't do is corrupt
  the workflow: malformed calls are unrepresentable, invalid ones are
  rejected and retried, loops are cut deterministically. Quality degrades;
  integrity doesn't.
- **What about latency of validation?** Schema validation is microseconds
  against model inference in milliseconds-to-seconds; it's noise.
- **Extensions in other languages?** Anything that speaks MCP over stdio —
  the example is Python; TypeScript works the same way. WASM in-process
  extensions are the staged next mechanism (ADR 0006).
- **Production-ready?** Honest status: kernel, both adapters, demo slice
  and extension slice work end-to-end with receipts in the repo
  (`docs/BUILD-EVIDENCE.md`); persistence and memory are in-memory
  reference impls behind stable traits. It's a runtime you can extend, not
  a finished platform.
- **Privacy?** Fully local mode: llama.cpp sidecar + kernel on your
  machine, no external calls. Frontier mode uses your own OpenRouter key
  from the environment.

## Gallery shot list

Matches [demo-storyboard.md](demo-storyboard.md):

1. **Hero GIF** — side-by-side one-line-swap run (Scene 1).
2. **Extend-in-Python GIF** — code → manifest → registered → model uses it
   (Scene 2).
3. Architecture card (README diagram, styled).
4. Reliability close-up — a rejected proposal + successful retry in the
   event log.
5. Topology card — one kernel: container / desktop / PWA-over-tailnet.
6. Testability — mock-oracle kernel test beside a green `cargo test`.

## Hero graphic spec (placeholder — not rendered in Phase 2)

Dark background (#101418), the kernel-as-ring motif from the app icon at
center; two engine glyphs (chip = local, cloud = frontier) feeding one ring;
one glowing config line beneath: `engine = "…"`. Wordmark **Kelter**
lowercase-k optional per final brand pass. 1270×760 for PH, keep a 16:9
master.

## Launch-day checklist (human)

- [x] Name decided: **Kelter** (2026-07-18)
- [ ] Handles/domain/crate/npm/GitHub-org secured *(gate prerequisite)*
- [ ] Record Scenes 1–3, cut hero GIF ≤ 30 s
- [ ] Maker-comment voice pass; positioning sign-off
- [ ] Re-verify quickstart on a clean machine
- [ ] Schedule (Tue–Thu, 00:01 PT) and submit
