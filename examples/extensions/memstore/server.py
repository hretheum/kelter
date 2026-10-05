"""Kelter memory extension example: a memory BACKEND in ~30 lines of Python.

Implements the reserved memory-capability tools (memory_write /
memory_retrieve / memory_upsert, plus the optional memory_episode_history).
The kernel talks to it over MCP and schema-validates every response — no
Rust, no recompilation.

ADR 0038 contract: a write of a stored `id` with a higher `version` is a
revision (every version kept), and an episode whose current version is
"unverified" is never retrieved — it must not reach model context.
"""

import json
from pathlib import Path

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("memstore")
DB = Path(__file__).parent / "memstore.json"


def load() -> dict:
    return json.loads(DB.read_text()) if DB.exists() else {"episodes": [], "graph": []}


def save(data: dict) -> None:
    DB.write_text(json.dumps(data, indent=2))


@mcp.tool()
def memory_write(episode: dict) -> dict:
    """Store one provenance-tagged episode."""
    data = load()
    data["episodes"].append(episode)  # every version is kept (history)
    save(data)
    return {"written": True}


def current(episodes: list) -> list:
    """The highest version of each episode id."""
    latest = {}
    for e in episodes:
        if e["id"] not in latest or e.get("version", 1) >= latest[e["id"]].get("version", 1):
            latest[e["id"]] = e
    return list(latest.values())


@mcp.tool()
def memory_retrieve(task: str, limit: int) -> dict:
    """Rank stored episodes against a task (simple term overlap)."""
    words = set(task.lower().split())
    results = []
    for e in current(load()["episodes"]):
        if e.get("outcome") == "unverified":
            continue  # never in model context (ADR 0038)
        hits = len(words & set(e.get("task", "").lower().split()))
        if hits:
            results.append({
                "episode_id": e["id"],
                "summary": e.get("task", ""),
                "score": min(1.0, hits / max(1, len(words))),
            })
    results.sort(key=lambda r: -r["score"])
    return {"results": results[:limit]}


@mcp.tool()
def memory_episode_history(run_id: str) -> dict:
    """Every stored version of a run's episodes, oldest first."""
    eps = [e for e in load()["episodes"] if e.get("run_id") == run_id]
    eps.sort(key=lambda e: (e["id"], e.get("version", 1)))
    return {"episodes": eps}


@mcp.tool()
def memory_upsert(item: dict) -> dict:
    """Store a graph entity or relation."""
    data = load()
    data["graph"].append(item)
    save(data)
    return {"upserted": True}


if __name__ == "__main__":
    mcp.run()  # stdio transport by default
