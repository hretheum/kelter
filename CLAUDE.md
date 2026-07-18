# CLAUDE.md — working conventions for this repo

**Kelter** — an agent-orchestration runtime: a deterministic kernel that
validates what a model proposes, model-agnostic via swappable policy adapters.
This file orients any Claude/agent working in the repo. Read it first.

## Where things live (keep the root clean)

- **`prompts/`** — internal working briefs and prompt inputs: the phase prompts
  for the autonomous build agent, and the assumptions / founding doc. **These
  live here, never at the repo root**, so the root does not fill up with loose
  `.md` files. The folder is **gitignored** — briefs are inputs, not artifacts,
  and are never committed. Drop any new brief or prompt here.
- **`docs/`** — committed project documentation: `architecture.md`, `architecture-overview.md` (diagrammed synthesis), ADRs
  (`docs/adr/`), `BUILD-EVIDENCE.md`, `PHASE-1-RESULTS.md`, launch assets
  (`docs/launch/`), and raw run logs (`docs/evidence/`).
- **`crates/` · `apps/` · `deploy/`** — code and deployment artifacts.

## Hard rules

- **Naming.** The product is **Kelter**. The pre-rename working codename — the
  one this repo's own directory is named after — must never appear inside any
  tracked file; `git grep -i` for it stays at 0. The interim placeholder
  `agentkernel` is fully renamed to `kelter` and now survives ONLY inside the
  pre-rename run logs under `docs/evidence/` and the notes that explain them.
  Those are historical audit records — **do not scrub them.**
- **Human gate.** No `git push`, no making the repo public, no Product Hunt
  submission, no crate / npm / GitHub-org / domain registration. Those are the
  maintainer's acts, not an agent's.
- **Secrets** live in the environment only (`OPENROUTER_API_KEY`), never in the
  tree; mask them in logs.
- **Provenance.** `.md` files an agent authors carry `provenance: ai_authored`
  frontmatter (with `provenance_metadata`); untagged files are human-authored.
