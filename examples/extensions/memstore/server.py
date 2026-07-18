"""Kelter memory extension example: a memory BACKEND in ~30 lines of Python.

Implements the reserved memory-capability tools (memory_write /
memory_retrieve / memory_upsert). The kernel talks to it over MCP and
schema-validates every response — no Rust, no recompilation.
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
    data["episodes"].append(episode)
    save(data)
    return {"written": True}


@mcp.tool()
def memory_retrieve(task: str, limit: int) -> dict:
    """Rank stored episodes against a task (simple term overlap)."""
    words = set(task.lower().split())
    results = []
    for e in load()["episodes"]:
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
def memory_upsert(item: dict) -> dict:
    """Store a graph entity or relation."""
    data = load()
    data["graph"].append(item)
    save(data)
    return {"upserted": True}


if __name__ == "__main__":
    mcp.run()  # stdio transport by default
