---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-4-8
  session_date: 2026-07-19
  human_validated: false
  session_context: "Phase 8, item G: desktop packaging scaffolding — bundle targets, universal build, and the signing/notarization steps that are human-blocked (need an Apple Developer ID cert)."
---

# Packaging the desktop shell

The Kelter desktop shell (`apps/desktop`) is a Tauri v2 app. Bundle targets and
signing hooks live in `apps/desktop/src-tauri/tauri.conf.json`; the build is
driven by `scripts/package-desktop.sh`.

## Build (unsigned)

```sh
scripts/package-desktop.sh                 # host-native .app + .dmg, unsigned
scripts/package-desktop.sh --universal     # macOS universal (arm64 + x86_64)
scripts/package-desktop.sh --bundles app   # .app only (skip dmg)
```

Configured targets (`bundle.targets`): `app`, `dmg` (macOS), `nsis` (Windows),
`deb`, `appimage` (Linux). Tauri builds the targets applicable to the host OS —
Windows/Linux installers build on those hosts or in CI with the same config.

The script produces **unsigned** artifacts by design: `tauri.conf.json` sets
`bundle.macOS.signingIdentity = null` and `bundle.windows.certificateThumbprint
= null`. Nothing in this repo signs anything.

### Known limitation — headless `.dmg`

`.dmg` creation runs `bundle_dmg.sh`, which needs a macOS **GUI session** (it
scripts Finder to lay out the disk image). In a headless/CI shell the `.dmg`
step fails even though the `.app` builds fine; build the `.dmg` from a desktop
session, or build `--bundles app` in CI and create the `.dmg` on a UI runner.

## Signing + notarization — HUMAN STEPS (not done here)

Signing and notarization require an **Apple Developer ID certificate**, a paid
credential held by a person — an agent must not (and here does not) sign. When
the maintainer is ready to distribute:

1. Obtain a **Developer ID Application** certificate (Apple Developer Program)
   and install it in the login keychain.
2. Set the signing identity (either in `tauri.conf.json` →
   `bundle.macOS.signingIdentity`, or via env for the build):
   ```sh
   export APPLE_SIGNING_IDENTITY="Developer ID Application: <Name> (<TEAMID>)"
   ```
3. Notarize with an app-specific password (Apple ID) or an API key:
   ```sh
   export APPLE_ID="you@example.com"
   export APPLE_PASSWORD="app-specific-password"
   export APPLE_TEAM_ID="<TEAMID>"
   ```
   Tauri staples the notarization ticket when these are present.
4. Re-run `scripts/package-desktop.sh` — it will now produce a signed, notarized
   `.dmg`.

Windows signing (`bundle.windows.certificateThumbprint` + a code-signing cert)
is the analogous human step on Windows.

## Deploy to the K12 homelab host — HUMAN STEP (hardware pending)

The headless daemon ships as a Podman Quadlet (`deploy/kelter.container`),
validated locally with `scripts/validate-quadlet.sh`. Deploying it to the K12
host is deferred until that hardware arrives; it is a human step (copy the unit,
`systemctl --user daemon-reload`, start, verify) — not done here.
