"""A toy A2A agent — the DELEGATION TARGET for Kelter's outbound A2A demo.

Minimal A2A v1.0.0 JSON-RPC server (Starlette, no SDK) exposing an Agent
Card and the two methods Kelter's client uses: SendMessage (returns a Task)
and GetTask (poll to completion). Deliberately dependency-light and
offline (localhost only).

This is NOT Kelter-as-a-server — it's an external agent Kelter delegates to.

  pip install starlette uvicorn
  python server.py --port 7590 [--slow N]   # --slow N: N WORKING polls before COMPLETED
"""

import argparse
import json
import sys
import uuid

from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

TASKS: dict[str, dict] = {}
SLOW = 0  # GetTask polls to stay WORKING before completing


def agent_card(request: Request) -> JSONResponse:
    base = str(request.base_url).rstrip("/")
    return JSONResponse({
        "name": "Toy Echo Agent",
        "description": "Uppercases the delegated text — a stand-in remote agent.",
        "supportedInterfaces": [
            {"url": f"{base}/rpc", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
        ],
        "version": "1.0.0",
        "capabilities": {"streaming": False},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{"id": "echo", "name": "Echo", "description": "Uppercase echo",
                    "tags": ["demo"]}],
    })


def _text_of(message: dict) -> str:
    return " ".join(p.get("text", "") for p in message.get("parts", []))


async def rpc(request: Request) -> JSONResponse:
    req = await request.json()
    rid, method, params = req.get("id"), req.get("method"), req.get("params", {})

    def ok(result):
        return JSONResponse({"jsonrpc": "2.0", "id": rid, "result": result})

    def err(code, msg):
        return JSONResponse({"jsonrpc": "2.0", "id": rid,
                             "error": {"code": code, "message": msg}})

    if method == "SendMessage":
        task_id = str(uuid.uuid4())
        answer = _text_of(params["message"]).upper()
        # Model the task as needing SLOW polls before it completes.
        TASKS[task_id] = {"remaining": SLOW, "answer": answer}
        state = "TASK_STATE_COMPLETED" if SLOW == 0 else "TASK_STATE_SUBMITTED"
        artifacts = ([{"artifactId": str(uuid.uuid4()), "parts": [{"text": answer}]}]
                     if SLOW == 0 else [])
        return ok({"task": {"id": task_id, "status": {"state": state},
                            "artifacts": artifacts}})

    if method == "GetTask":
        task = TASKS.get(params["id"])
        if task is None:
            return err(-32001, "TaskNotFoundError")
        if task["remaining"] > 0:
            task["remaining"] -= 1
            return ok({"id": params["id"], "status": {"state": "TASK_STATE_WORKING"},
                       "artifacts": []})
        return ok({"id": params["id"], "status": {"state": "TASK_STATE_COMPLETED"},
                   "artifacts": [{"artifactId": str(uuid.uuid4()),
                                  "parts": [{"text": task["answer"]}]}]})

    return err(-32601, f"method not found: {method}")


app = Starlette(routes=[
    Route("/.well-known/agent-card.json", agent_card),
    Route("/rpc", rpc, methods=["POST"]),
])


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=7590)
    ap.add_argument("--slow", type=int, default=0)
    args = ap.parse_args()
    SLOW = args.slow
    import uvicorn
    print(f"toy A2A agent on :{args.port} (slow={SLOW})", file=sys.stderr)
    uvicorn.run(app, host="127.0.0.1", port=args.port, log_level="warning")
