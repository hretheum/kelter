"""Kelter extension example: a new agent tool in ~25 lines of Python.

No Rust, no recompilation — this is a standard MCP server (official `mcp`
SDK); the manifest next to it registers it with the runtime at startup.
"""

import json
import os
import re
from collections import Counter
from pathlib import Path

from mcp.server.fastmcp import FastMCP

mcp = FastMCP("wordstats")
WORKSPACE = Path(os.environ.get("KELTER_WORKSPACE", "."))


@mcp.tool()
def word_stats(source: str, save_as: str = "stats.json") -> dict:
    """Compute word statistics (count, unique, top words) for a text file
    and save them as JSON in the workspace under save_as."""
    path = Path(source) if Path(source).is_absolute() else WORKSPACE / source
    words = re.findall(r"[A-Za-z']+", path.read_text().lower())
    stats = {
        "words": len(words),
        "unique": len(set(words)),
        "top5": Counter(words).most_common(5),
        "saved_as": save_as,
    }
    (WORKSPACE / save_as).write_text(json.dumps(stats, indent=2))
    return stats


if __name__ == "__main__":
    mcp.run()  # stdio transport by default
