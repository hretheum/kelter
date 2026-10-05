---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-04
  human_validated: false
  session_context: "K12 acceptance (2026-10-04): one dropped subprocess extension without operator consent aborted the daemon start and put systemd into a restart loop. Owner decision: such an extension is skipped and the daemon starts; ADR 0012's security posture is unchanged."
---

# ADR 0035: A refused extension is skipped, not fatal to the daemon

Status: accepted · Date: 2026-10-04 · Amends: ADR 0012 (enforcement — *how* a refusal is expressed, not *what* is refused) · Builds on: ADR 0008 (registry) · Trigger: K12 acceptance

## Context

On K12 an extension directory `wordstats` declared
`trust = "subprocess-unsandboxed"` but was not listed in the operator's
`[trust] allow_unsandboxed_subprocess`. ADR 0012 says the registry must refuse
it, and it did — by aborting the whole daemon:

```
Error: extension 'wordstats' opted into subprocess-unsandboxed, but the operator has not allowed it…
```

Exit 1, and systemd restarted it into the same error forever. The same was true
of any manifest error: one unparseable `kelter-extension.toml` anywhere under
`extension_dirs` stopped the agent. A single dropped directory — something any
process that can write into the extensions folder can do — was a denial of
service against the whole runtime. The refusal was correct; its blast radius
was not.

## Decision

**A refused extension is skipped, loudly; the daemon starts with the rest.**

1. **What is refused is unchanged.** An un-allowed `subprocess-unsandboxed`
   extension is still never started; every manifest rule (unknown capability,
   unknown permission, missing trust declaration, WASM trust mismatch, …) still
   refuses. ADR 0012's two-part opt-in — manifest declares, operator allows —
   is untouched. The manifest still cannot authorise its own escape. Skipping
   *is* the refusal, scoped to the one extension.
2. **Registry.** `mcp::extensions::resolve(dirs, allow_unsandboxed)` applies
   both manifest validation and the operator gate and returns a `Registry`:
   the extensions to wire up, plus a list of `SkippedExtension { name?,
   manifest, reason, detail, remedy }`. Reasons: `invalid_manifest` (the file
   could not be read, parsed or validated — `name` is `null` when it was too
   broken to name the extension) and `operator_consent_required`. It never
   fails. The daemon logs each skip at **ERROR** with the extension name, the
   manifest path, the reason and the remedy — for consent, the exact
   `[trust] allow_unsandboxed_subprocess` entry to add, with the reminder that
   allowing it does **not** sandbox the subprocess.
3. **Defence in depth.** The daemon keeps its own operator-consent check in
   the wiring loop. If an unconsented subprocess ever reaches it, the registry
   broke its contract and the daemon refuses to start — a fail-closed tripwire,
   not the normal path.
4. **Visibility: an additive field on `GET /api/v1/tools`.** The response gains
   `skipped_extensions: [...]`, always present (`[]` when nothing was skipped).
   - *Why this endpoint, not a new one.* `/api/v1/tools` already answers "what
     did the registry give the model?" — its doc comment calls it the way to
     verify "an extension registered without recompiling anything". The
     operator who wonders why their tool is missing looks here first, and finds
     the answer next to the list it is missing from. A separate
     `/api/v1/extensions` endpoint would be the better home for a full
     extension inventory (loaded ones with trust posture included), but that is
     a larger surface than this fix needs; it can absorb this field later.
   - *Why it is safe to add.* Additive JSON: existing clients that read only
     `tools` (the web UI's `loadTools`, the desktop shell) are unaffected. The
     endpoint stays bearer-gated, so manifest paths and refusal text are shown
     only to an authenticated principal.
   - The `api` crate carries its own `SkippedExtension` DTO rather than
     depending on `mcp`: the HTTP contract crate stays independent of the tool
     substrate. It is wired via new `router_full_with_skipped` /
     `build_app_full_with_skipped` entry points; the existing builders are
     unchanged (they report `[]`), matching the repo's additive-builder
     convention.
5. **Daemon configuration errors still abort.** An unreadable or invalid
   daemon config, a bad `[trust]`/`[tools]` value, an unbindable port, a
   missing TLS cert, two memory backends — the operator's own file, not a
   dropped directory — still stop the start. So does "no action sources
   configured": if the skipped extensions were the only action sources, the
   daemon has nothing to offer the model, which is a configuration error; its
   message now names the skipped extensions so the cause is not hidden.

## Consequences

- One bad extension directory degrades one capability instead of disabling the
  agent. The K12 restart loop cannot recur from a manifest or consent refusal.
- Security posture is identical: the same set of extensions is refused, and a
  refused subprocess is never spawned (the daemon test asserts its marker file
  is never created).
- A refusal is now *quieter by default* than an exit — a service that is up
  looks healthy. Two things keep it from being silent: the ERROR log line and
  the API field. A monitoring check on `skipped_extensions != []` is the
  natural alert; the web UI does not surface it yet (backlog).
- `scripts/smoke-trust.sh` legs A/B now assert "daemon up + ERROR + reported
  skipped + never started" instead of "daemon exits non-zero".

**Out of scope, deliberately.** A *valid* manifest whose WASM module then
fails to load (missing file, ungranted import) still aborts the start
(`failed to load WASM extension …`). It is the same class of problem — one
directory taking the daemon down — but it is a load-time failure of an
accepted extension, not a registry refusal, and the owner's decision covered
the registry. It is the obvious next candidate for the same rule.

## Evidence

- `crates/mcp/src/extensions.rs` — `resolve_skips_untrusted_subprocess_and_broken_manifest`,
  `resolve_registers_subprocess_the_operator_allowed`,
  `resolve_skips_invalid_manifest_with_its_name`.
- `crates/daemon/tests/extension_skip_test.rs` — spawns the real `kelter`
  binary: WASM tool + un-allowed subprocess + broken manifest → daemon healthy,
  `word_count` offered, two skips in `/api/v1/tools`, two ERROR lines, the
  subprocess never ran; and with the `[trust]` entry the subprocess registers
  and its tools are offered.
- `scripts/smoke-trust.sh` (legs A–D) — run by hand, PASS.
- `docs/BUILD-EVIDENCE.md`, 2026-10-04 entry.
