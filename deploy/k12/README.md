---
provenance: ai_authored
provenance_metadata:
  agent: claude-code
  session_date: 2026-10-05
  human_validated: false
  session_context: "Decision D19: one canonical Quadlet unit in the repo (deploy/kelter.container), deployed to K12 unchanged; everything K12-specific lives in these drop-ins. Values taken from the unit K12 ran through acceptance on 2026-10-05 (29/29 scenarios, twice in a row)."
---

# K12 deployment (rootless Podman, Fedora CoreOS)

K12 runs the **canonical** unit `deploy/kelter.container`, byte for byte. What
is specific to K12 is three Quadlet drop-ins in
[`kelter.container.d/`](kelter.container.d/):

| Drop-in | Sets | Why it is K12's, not the canonical unit's |
|---|---|---|
| `10-image.conf` | `Image=ghcr.io/hretheum/kelter:sha-<commit>` | the image K12 pulls, pinned by commit SHA — never `:latest` |
| `20-network.conf` | `Network=llm-net` | shared podman network with llama-swap; `host.containers.internal` does not reach services on the host's loopback |
| `30-userns.conf` | `UserNS=keep-id:uid=999,gid=999` | rootless: lets the image's uid 999 read the 0600 config (bearer token) without loosening it; keep-id is rootless-only |

Everything else K12 needs is already the canonical default: loopback-only
publish (`127.0.0.1:7466`, reach it with `ssh -L 7466:127.0.0.1:7466 k12`), the
`kelter-state` volume, `LogDriver=journald`, `Memory=512M`, the health check
(`HealthStartPeriod=20s`, `HealthOnFailure=kill`), `StartLimitIntervalSec=0`,
`Restart=on-failure` + `RestartSec=10`. No `EnvironmentFile=`: K12 runs a local
engine (an empty `~/.config/kelter/env` may exist; nothing reads it).

Drop-ins merge in file-name order. `Image=` and `UserNS=` are single-value —
the drop-in wins. `Network=` is a list key and only adds, which is why the
canonical unit has none.

## Install

On K12, as the user that runs the unit (`core`, lingering enabled by
`deploy/fcos/k12.bu`), from a checkout of this repo:

```sh
mkdir -p ~/.config/containers/systemd
cp deploy/kelter.container ~/.config/containers/systemd/
rm -rf ~/.config/containers/systemd/kelter.container.d
cp -r deploy/k12/kelter.container.d ~/.config/containers/systemd/

/usr/libexec/podman/quadlet -dryrun -user >/dev/null   # must not print an error
systemctl --user daemon-reload
systemctl --user restart kelter
scripts/check-quadlet.sh            # 0 = installed unit equals repo + drop-ins
```

The `rm -rf` matters: a stale drop-in left behind on the host (an old file
name) would still be merged. Nothing else is edited by hand on the host.

Prerequisites that live outside the unit (unchanged by this directory):

- `~/.config/kelter/config.toml` (0600), with at least
  `[storage] path = "/data/state/kelter.db"` — otherwise the store is not on
  the `kelter-state` volume and is lost with every container restart — and
  `[tools] extension_dirs = ["/data/config/extensions"]` for the extensions in
  `~/.config/kelter/extensions/` (see [`../extensions/README.md`](../extensions/README.md)).
- the `llm-net` podman network (created with llama-swap).
- journald's cap: `SystemMaxUse=` set explicitly (see
  [`docs/observability.md`](../../docs/observability.md#bounding-log-size-deployment-adr-0017)).

## Memory (2026-10-13)

The host has no disk swap. Two settings keep a memory spike from taking out
the wrong thing:

- **zram swap**, 8 GiB zstd (`/etc/systemd/zram-generator.conf` on the host:
  `[zram0] zram-size = min(ram / 4, 8192)`, `compression-algorithm = zstd`,
  `swap-priority = 100`). A peak slows things down instead of triggering the
  OOM killer at once. Not in `deploy/fcos/k12.bu`, which predates the
  current host (user `core`, stock FCOS) — set by hand.
- **OOM priorities** (`--oom-score-adj`; rootless containers default to 200,
  higher dies first): Kelter `100` (`kelter.container.d/40-oom.conf`, this
  repo); claude-code-runner `800` and clm `1000` (their units in
  `deploy/claude-code/`); on the host, outside this repo, drop-ins
  `50-oom.conf` give `100` to Home Assistant, mosquitto, matter-server,
  Ghost + its DB, hretheum-app, hretheum-db and both cloudflared tunnels, and
  `700` to hretheum-embeddings.
- **Lowering needs the unit, not podman:** an unprivileged process can only
  raise its `oom_score_adj`, and a user service starts at 200 — so a value
  *below* 200 must be `[Service] OOMScoreAdjust=` (the user manager may set
  it, it sits at 100); `--oom-score-adj=100` alone is silently ignored. The
  first version did exactly that and the protected services came back at 200
  after the 2026-10-13 reboot; raising (700/800/1000) works with podman
  alone.

## Upgrading the image = one drop-in

Bump the SHA in `kelter.container.d/10-image.conf`, commit it, then on K12:

```sh
cp deploy/k12/kelter.container.d/10-image.conf ~/.config/containers/systemd/kelter.container.d/
podman pull ghcr.io/hretheum/kelter:sha-<commit>   # optional: pull before the restart
systemctl --user daemon-reload
systemctl --user restart kelter
scripts/check-quadlet.sh
```

The tag must exist on GHCR (`publish-image.yml` tags every published image
`sha-<short commit>`). Rolling back is the same edit with the previous SHA.

## Checking for drift

`scripts/check-quadlet.sh` renders the repo unit + these drop-ins and the
unit installed on the host with the **host's** quadlet generator and compares
the resulting `podman run` command (flag order ignored) and the generated
`[Unit]`/`[Service]` lines. Exit `0` = equivalent, `1` = drift (prints a
diff), `2` = could not check.

```sh
scripts/check-quadlet.sh                  # on K12
scripts/check-quadlet.sh --ssh core@k12   # from a workstation with this checkout
```
