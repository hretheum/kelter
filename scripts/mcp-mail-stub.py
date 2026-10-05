#!/usr/bin/env python3
# A minimal MCP stdio server exposing one tool, `send_email`, for
# scripts/smoke-success-contract.sh. It exists so a success contract can name
# `send_email` (the contract is validated against the live action space) while
# the scripted model never calls it. If it IS called, it records the call in
# sent.log (cwd) and reports {"status": "sent"}.
import json, sys

def reply(req_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result}) + "\n")
    sys.stdout.flush()

TOOL = {
    "name": "send_email",
    "description": "Send an e-mail to a recipient.",
    "inputSchema": {
        "type": "object",
        "properties": {"to": {"type": "string"}, "body": {"type": "string"}},
        "required": ["to", "body"],
    },
}

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, req_id = msg.get("method"), msg.get("id")
    if req_id is None:
        continue  # notification (e.g. notifications/initialized)
    if method == "initialize":
        reply(req_id, {
            "protocolVersion": msg["params"].get("protocolVersion", "2025-03-26"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mail-stub", "version": "0.1.0"},
        })
    elif method == "tools/list":
        reply(req_id, {"tools": [TOOL]})
    elif method == "tools/call":
        with open("sent.log", "a") as f:
            f.write(json.dumps(msg["params"].get("arguments", {})) + "\n")
        out = {"status": "sent"}
        reply(req_id, {"content": [{"type": "text", "text": json.dumps(out)}],
                       "structuredContent": out, "isError": False})
    elif method == "ping":
        reply(req_id, {})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": req_id,
                                     "error": {"code": -32601, "message": "method not found"}}) + "\n")
        sys.stdout.flush()
