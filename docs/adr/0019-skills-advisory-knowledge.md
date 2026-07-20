---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-20
  human_validated: false
  session_context: "Skills subsystem: OpenSkills/SKILL.md procedural knowledge as advisory oracle context — never authorizing; capabilities only via the existing sandbox-by-default registry."
---

# ADR 0019: Skills — advisory procedural knowledge, never authorization

Status: accepted · Date: 2026-07-20 · Builds on: ADR 0001 (kernel as runtime),
ADR 0006 (governed extensions), ADR 0008 (capability registry), ADR 0012
(sandbox-by-default trust)

## Context

The OpenSkills / Anthropic `SKILL.md` format packages *procedural knowledge*:
a Markdown body of instructions with `name`/`description` frontmatter,
optionally alongside `scripts/`, references and assets. A large public corpus
exists (anthropics/skills, OpenSkills, skillforge bundles). Kelter should let
**anyone add any skill** — but Kelter's entire thesis is that a deterministic
kernel validates everything a model proposes. A skills subsystem that became a
new authority (skills granting tools, activation running scripts) would break
the thesis at its core.

## Decision

A skill is **advisory, never authorizing**. It decomposes onto two EXISTING
planes and adds no new intermediary to the propose→validate→execute loop:

1. **Knowledge → the oracle's context.** `SkillContextOracle` (crate
   `skills`) is a `PolicyOracle` decorator: it appends skill context to
   `PolicyContext.guidance` and delegates — **passing the kernel's
   `ActionSchema` through untouched**. There is no code path in the crate
   that constructs or edits a schema, so skill content structurally cannot
   widen the action space. Progressive disclosure is mandatory and budgeted:
   the catalog (name+description, ≤8 KiB) is always visible; full
   instructions (≤16 KiB per skill, ≤48 KiB total) enter only for skills the
   operator activated. In the daemon the decorator wraps the
   `SwappableOracle`, so runtime model swaps keep the layer.
2. **Capabilities → the existing registry, as candidates only.** Bundled
   `scripts/` or a `kelter-extension.toml` are surfaced by `skill list` as
   *candidates*; they become actions exclusively through `[tools]
   extension_dirs` + the ADR-0012 gate (sandbox-by-default; unsandboxed
   subprocess needs the manifest declaration AND the operator's `[trust]`
   allowlist — the unchanged two-man opt-in). Install/activation never
   executes anything; execution remains `ValidatedAction`-only.

**Why "anyone adds any skill" is safe:** activation adds knowledge, not
capability. Whatever a skill urges, the kernel re-gates every proposal
against the same action schema. The residual risk is *steering* — a skill can
push the model toward an allowed-but-unwanted action. That is exactly why
the action space stays tight and why per-skill **provenance** is recorded by
the subsystem (`.kelter-skill.toml` sidecar: source, install time, trust —
`untrusted` by default) and surfaced. Frontmatter claims of trust/provenance
are ignored by design: an adversarial SKILL.md cannot self-elevate, and the
frontmatter parser is a deliberately minimal `key: value` subset (no YAML
engine — nothing for hostile YAML to exploit; malformed input loses
metadata, never gains authority).

**Open installation.** `kelter skill add <dir|bundle|https://…tar.gz>` +
`list/remove/activate/deactivate`; state lives in files under `[skills] dir`,
mtime-cached, so CLI activation takes effect in a running daemon without a
restart. Activation is also seedable from config (`[skills] active`).

## Consequences

- Proven invariants (test suite + live evidence): a skill urging an
  out-of-space action is proposed by the model and **rejected by the kernel**;
  inactive bodies never reach the oracle; bundled scripts do not run on
  install/activation/context-build; 265 skillforge-ECC skills plus arbitrary
  OpenSkills-format folders install with zero special-casing, all
  `untrusted`.
- The kernel, `ValidatedAction`, and the registry gates are untouched — the
  subsystem is a policy-side decorator plus file bookkeeping.
- Deferred: exposing skills in the web UI (trust/provenance surfaced there),
  per-run skill selection, catalog compression for constrained-latitude
  engines, and signed/trusted skill sources.
