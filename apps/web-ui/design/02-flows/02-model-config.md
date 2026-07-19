<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — flow spec: model/policy configuration, including the read-only (501) variant and SSRF-refusal outcome. -->

# Flow 02 — Model / policy configuration

**Job:** J5. **Destination:** Settings (S-1), off the primary loop. **API:**
`GET/POST /api/v1/config/model`, `POST …/test`, `POST …/key`.

## Entry

- Top-bar model chip click, or ⚙ (Settings), or `Cmd/Ctrl+,`.
- First-run routing sent here (Flow 01) when no reachable model.

## Steps (mutable / 200)

1. **Load** — `GET /config/model` fills the form: provider (Local/API), preset
   (llama.cpp / ollama / OpenRouter / custom), base URL, model, and the key row
   (API providers only). `api_key_set:true` → key field shows "leave blank to
   keep the stored key."
2. **Pick** — choosing a preset pre-fills base URL + model + provider (from
   `CFG_PRESETS`). Custom lets all fields be edited.
3. **Test connection** — `POST …/test {provider, base_url}`. Real probe:
   - `ok:true` → "✓ Reachable — {status} {url}" (green).
   - `ok:false` reachable-but-bad → "✗ {status} from {url}" (red).
   - `ok:false` **allowlist refusal** → "✗ Refused — this address isn't on the
     connection allowlist (presets + your configured base URL)." (distinct, honest
     — the SSRF guard, not a generic failure).
4. **Save the key first** (if entered) — `POST …/key`; on 204 clear the field,
   flip `api_key_set` note. The value is never echoed.
5. **Save the model** — `POST /config/model {engine, base_url}`. 200 → "Saved.
   Model configured." Hot-swap is live; the **next** run uses it. Return to Runs.
6. **422** on save → show the reason inline (e.g. "openrouter needs an API key").

## Steps (immutable / 501)

- `GET` returns 501 → the whole form renders **read-only/disabled** with a note:
  "This deployment's model is fixed at deploy time — not editable here." No Test,
  no Save. Matches today's `setConfigEditable(false, …)`. The user is not lied to
  with a live-looking form that would reject Save.

## States

| State | Rendering |
|---|---|
| loading | skeleton fields + spinner |
| editable (200) | full form active |
| testing | Test button → spinner "Testing…", form stays editable |
| test-ok / test-failed / test-refused | inline result line (green/red), 3 distinct texts |
| saving | Save → spinner "Saving…" |
| saved | success line; auto-return to Runs after confirm |
| error (422) | inline reason under the offending field |
| **read-only (501)** | all inputs disabled + deploy-time note |

## Empty / error

- No key stored, API provider selected → key field empty with the "stored
  server-side, never logged" note; not an error.
- Test before base URL entered → inline "Enter a base URL first."

## Keyboard

- `Cmd/Ctrl+,` opens Settings; `Esc` returns to Runs (discarding unsaved edits
  after a confirm if the form is dirty); `Cmd/Ctrl+S` = Save (when enabled).

## Exit

Back to Runs with the model chip updated. Or, in 501, back to Runs having
understood the model is fixed.
