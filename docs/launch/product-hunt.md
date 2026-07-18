# Product Hunt launch — DRAFT (finalize in Phase 2)

> Status: working draft. Copy, naming and all visuals get a dedicated pass
> before launch; nothing here ships as-is. Product name is the working
> placeholder "agentkernel".

## Tagline candidates

1. *Agent orchestration that survives weak models.*
2. *The kernel is the runtime. The model is just the policy.*
3. *Swap GPT for a 0.5B local model — same agent, one config line.*
4. *Deterministic agents: the model proposes, the kernel disposes.*
5. *Stop letting tool-call luck run your agents.*

Working favorite: **1**, with **3** as the gallery hook.

## First comment / maker comment (sketch)

> Every agent framework I tried had the same failure mode: the moment the
> model fumbled a tool call, the whole workflow fell over. The model *was*
> the runtime.
>
> agentkernel inverts that. A deterministic Rust kernel owns control flow —
> retries, validation, persistence — and the LLM is a constrained policy
> oracle. Locally, a GBNF grammar makes malformed tool calls literally
> unrepresentable at the decoder. Remotely, native tool-calling does the
> same job. The kernel re-validates everything either way.
>
> The demo: one real multi-step task (read → extract → report over MCP
> tools) runs identically on a 0.5B local model and a frontier model —
> the swap is one line of config. That's the whole thesis: model-agnostic,
> gracefully degrading, actually testable.
>
> It's OSS (MIT/Apache-2.0), Rust, MCP-native, one kernel for server /
> desktop / PWA. I'd love brutal feedback on the adapter contract.

To finalize: personal voice pass, link list, exact model names from the
recorded demo.

## Gallery shot list

1. **Hero (GIF)** — split screen: same task streaming live in the web UI,
   left `local:qwen2.5-0.5b`, right `openrouter:<frontier>`; config diff
   (one line) overlaid between them. *The money shot.*
2. **The one-line swap** — terminal: `diff config-local.toml
   config-openrouter.toml` showing a single `engine =` line changing.
3. **Reliability mechanics** — event log close-up: an invalid proposal
   rejected by kernel validation, retried with feedback, then succeeding.
4. **Architecture card** — the layer diagram from the README, styled.
5. **Topology card** — one kernel: container / desktop / PWA-over-tailnet.
6. **Testability** — the mock-oracle kernel test file next to a green
   `cargo test` run.

Hero graphic: spec only for now — dark background, kernel-as-ring motif
(matches app icon), two engine arrows converging on one action schema.
No rendered assets in Phase 1.

## Launch-day checklist (Phase 2)

- [ ] Final name + domain + handle check
- [ ] Record side-by-side GIF (≤30 s, loops cleanly)
- [ ] README polish pass (badges, quickstart re-verified on a clean machine)
- [ ] Maker comment final voice pass
- [ ] Schedule launch (Tue–Thu, 00:01 PT)
- [ ] Prepared answers: "why not <framework>?" (answer on positioning, not
      naming competitors), "what about latency?", "local model quality?"
