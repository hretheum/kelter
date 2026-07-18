#!/bin/sh
# Self-bootstrapping launcher for the toy A2A agent. Creates a private venv
# with Starlette+uvicorn on first run, then execs the server. Setup output
# goes to stderr.
cd "$(dirname "$0")" || exit 1

find_python() {
  for p in python3.13 python3.12 python3.11 python3.10 python3; do
    command -v "$p" >/dev/null 2>&1 || continue
    if "$p" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3,10) else 1)' 2>/dev/null; then
      echo "$p"; return 0
    fi
  done
  return 1
}

if [ ! -x .venv/bin/python ]; then
  PY=$(find_python) || { echo "toy A2A agent needs Python >= 3.10" 1>&2; exit 1; }
  "$PY" -m venv .venv 1>&2 || exit 1
  .venv/bin/pip install --quiet "starlette>=0.37" "uvicorn>=0.30" 1>&2 || exit 1
fi
exec .venv/bin/python server.py "$@"
