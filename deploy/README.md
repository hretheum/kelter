---
provenance: ai_authored
provenance_metadata:
  agent: claude-fable-5
  session_date: 2026-07-18
  human_validated: false
  session_context: "Phase 3: deployment notes (compose, rootless podman/Quadlet)"
---

# Deploying Kelter

## Docker Compose (kernel + local model as one pod)

```sh
cd deploy && docker compose up
```

Persist runs across container restarts by keeping the SQLite file on a
volume: the shipped config puts it under `/data`, which compose mounts.

## Rootless Podman + systemd (Quadlet)

Run as an unprivileged user — no root daemon, no port-forwarding magic:

```sh
# on the podman host, as your user
podman build -t localhost/kelter:latest .
mkdir -p ~/.config/containers/systemd ~/.config/kelter
cp deploy/kelter.container ~/.config/containers/systemd/
cp deploy/config/config.toml ~/.config/kelter/
systemctl --user daemon-reload
systemctl --user start kelter
journalctl --user -u kelter -f
# survive logout: loginctl enable-linger $USER
```

Extensions: to add the deployable WASM extensions (e.g. `zapisz_plik`, a
sandboxed file writer), copy them under `~/.config/kelter/extensions/` and set
`[tools] extension_dirs = ["/data/config/extensions"]` — the existing config
mount carries them, no unit change. Details: [`extensions/README.md`](extensions/README.md).

`deploy/kelter.container` is the **one canonical unit** (decision D19): it is
installed unchanged everywhere, and whatever is specific to a deployment —
a pinned registry image, a shared podman network, `UserNS=keep-id`, an
`EnvironmentFile=` for the OpenRouter key — goes into Quadlet drop-ins in
`~/.config/containers/systemd/kelter.container.d/*.conf`. The K12 set, with
install and upgrade steps, is [`k12/`](k12/README.md). Its defaults: publish
on `127.0.0.1:7466` only (reach it over an ssh tunnel), the store on the
`kelter-state` volume (set `[storage] path = "/data/state/kelter.db"` in the
config), logs to journald.

Validation: `scripts/validate-quadlet.sh` runs static checks anywhere and
`quadlet -dryrun` (unit alone and unit + K12 drop-ins) where the generator is
available; CI runs it with a generator built from a pinned podman tag.
`scripts/check-quadlet.sh` compares the unit installed on a host with the
repo unit + drop-ins (on the host, or `--ssh core@k12`). **On a dev machine
without podman, validation prints the exact commands instead of pretending**
— running them on the real podman host is a human step.

Security posture (see `SECURITY.md`): bearer-token auth (constant-time
comparison), designed for localhost / private networks (tailnet); do not
expose publicly without TLS + rate limiting in front. The
`OPENROUTER_API_KEY` comes from an env file outside the repo
(`EnvironmentFile=` in the unit), never from the image.
