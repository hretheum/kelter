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

Validation: `scripts/validate-quadlet.sh` runs static checks anywhere and
`quadlet -dryrun` where podman is installed. **On a dev machine without
podman it prints the exact commands instead of pretending** — running them
on the real podman host is a human step.

Security posture (see `SECURITY.md`): bearer-token auth (constant-time
comparison), designed for localhost / private networks (tailnet); do not
expose publicly without TLS + rate limiting in front. The
`OPENROUTER_API_KEY` comes from an env file outside the repo
(`EnvironmentFile=` in the unit), never from the image.
