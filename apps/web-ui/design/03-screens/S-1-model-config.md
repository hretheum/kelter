<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — screen spec S-1 model configuration, including the read-only (501) variant. -->

# S-1 — Model configuration (Settings destination)

The only screen in Settings. Owns J5. Off the primary loop. Renders both the
mutable (200) and immutable (501) deployments honestly.

## Structure & hierarchy

```
┌─ Settings — Model ───────────────────────────  [ Done ⎋ ] ┐
│  Provider    ( Local ⦿   API ○ )                          │
│  Preset      [ llama.cpp (local)          ▾ ]             │
│  Base URL    [ http://127.0.0.1:8080         ]           │
│  Model       [ qwen2.5-0.5b-instruct         ]           │
│  API key     [ ················ ] (API only)             │  ← shown only for API providers
│              stored server-side, never logged            │
│  ───────────────────────────────────────────────────────  │
│  [ Test connection ]                      [   Save  ⌘S ]  │
│  ✓ Reachable — 200 http://127.0.0.1:8080/health          │  ← result line (3 variants)
└───────────────────────────────────────────────────────────┘
```

## Content (copy)

- Header: "Settings — Model". Return: "Done" (`Esc`).
- Provider: Local / API radio.
- Preset: llama.cpp (local) · Ollama (local) · OpenRouter (API) · Custom.
- Fields: Base URL, Model; API key (API only) with note: "Stored server-side,
  never committed or logged; or set `OPENROUTER_API_KEY`. Leave blank to keep the
  stored key." (`api_key_set:true`).
- Buttons: "Test connection", "Save".

## Variants / states

| State | Rendering |
|---|---|
| loading | skeleton fields + spinner |
| editable (200) | full form active |
| testing | Test → spinner "Testing…"; form stays editable |
| test-ok | green: "✓ Reachable — {status} {url}" |
| test-failed | red: "✕ {status} from {url}" |
| test-refused (SSRF) | red: "✕ Refused — this address isn't on the connection allowlist (presets + your configured base URL)." |
| saving | Save → spinner "Saving…" |
| saved | green "Saved. Model configured." → auto-return to Runs |
| error (422) | red inline under the offending field (e.g. "OpenRouter needs an API key") |
| **read-only (501)** | all inputs + buttons disabled; banner: "This deployment's model is fixed at deploy time — not editable here." |

## Behavior (from Flow 02)

- Preset change pre-fills Base URL + Model + Provider. Custom unlocks all.
- Save order: key (`POST …/key`, 204, clear field) → model (`POST …/config/model`,
  200). Hot-swap is live; next run uses the new model.
- Test hits the real provider (`POST …/test`); 3 outcomes above; allowlist refusal
  is distinct.

## Responsive

- Desktop: single column, comfortable field width (~420px).
- Narrow: full page; Done/`Esc` returns to Runs.

## Accessibility

- Every field a real `<label for>`; radios in a `<fieldset><legend>Provider`.
- Result line `role="status"` `aria-live="polite"`; error `aria-invalid` +
  `aria-describedby`.
- Read-only variant: `disabled` + `aria-disabled` + the banner announced once.
- Key field `type="password"`, `autocomplete="off"`.

## API

- `GET /api/v1/config/model` (200 view | 501 → read-only).
- `POST /api/v1/config/model {engine, base_url}` (200 | 422).
- `POST /api/v1/config/model/test {provider, base_url}` → `{ok, detail}`.
- `POST /api/v1/config/model/key {api_key}` → 204. Key never echoed.
