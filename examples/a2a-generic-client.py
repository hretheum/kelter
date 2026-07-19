#!/usr/bin/env python3
"""A generic A2A client — NO Kelter code, NO a2a-sdk — that proves Kelter's
inbound endpoint (ADR 0013) speaks real Agent-to-Agent, not a private protocol.

It does exactly what the A2A spec prescribes for a client:
  1. resolve the Agent Card at the well-known path
     (/.well-known/agent-card.json);
  2. read the declared JSON-RPC interface url and the bearer security scheme;
  3. call the task methods over JSON-RPC 2.0 with that auth
     (SendMessage / message/send, then GetTask / tasks/get).

Dependency-light on purpose (urllib only), offline, localhost — the same
"hand-rolled, spec-following" posture as Kelter's own outbound client (ADR
0009) and the 4b toy target. If this talks to Kelter, any conformant A2A
client does.

  python3 a2a-generic-client.py --base http://127.0.0.1:7601 --token TOKEN \
      --text "hello from a generic A2A client" [--method SendMessage]
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request

TERMINAL = {"TASK_STATE_COMPLETED", "TASK_STATE_FAILED", "TASK_STATE_CANCELED"}


def http_json(url, token=None, payload=None):
    headers = {"A2A-Version": "1.0", "Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(
        url, data=data, headers=headers, method="POST" if data else "GET"
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", required=True, help="origin, e.g. http://127.0.0.1:7601")
    ap.add_argument("--token", required=True, help="bearer token the card requires")
    ap.add_argument("--text", default="hello from a generic A2A client")
    # Default to the A2A v1.0 canonical name; the endpoint also accepts the
    # pre-1.0 spelling "message/send".
    ap.add_argument("--method", default="SendMessage")
    args = ap.parse_args()

    # 1. Discover the Agent Card at the well-known path.
    card = http_json(args.base.rstrip("/") + "/.well-known/agent-card.json")
    print("card.name:", card["name"])
    scheme = card.get("securitySchemes", {}).get("bearer", {}).get("scheme")
    print("card.securityScheme:", scheme)
    ifaces = card.get("interfaces") or card.get("supportedInterfaces") or []
    endpoint = next(i["url"] for i in ifaces if i.get("protocolBinding") == "JSONRPC")
    print("card.endpoint:", endpoint)

    # 2. message/send over JSON-RPC 2.0, authenticated with the declared scheme.
    send = http_json(
        endpoint,
        token=args.token,
        payload={
            "jsonrpc": "2.0",
            "id": "generic-1",
            "method": args.method,
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "ROLE_USER",
                    "parts": [{"text": args.text}],
                }
            },
        },
    )
    if "error" in send:
        print("send.error:", send["error"])
        sys.exit(1)
    task_id = send["result"]["task"]["id"]
    print("task.id:", task_id)

    # 3. Poll tasks/get to a terminal state.
    for _ in range(90):
        got = http_json(
            endpoint,
            token=args.token,
            payload={
                "jsonrpc": "2.0",
                "id": "generic-1",
                "method": "tasks/get",
                "params": {"id": task_id},
            },
        )
        state = got["result"]["status"]["state"]
        if state in TERMINAL:
            print("final.state:", state)
            arts = got["result"].get("artifacts", [])
            if arts and arts[0].get("parts"):
                print("final.artifact:", arts[0]["parts"][0].get("text", ""))
            sys.exit(0 if state == "TASK_STATE_COMPLETED" else 2)
        time.sleep(1)
    print("timeout waiting for a terminal state")
    sys.exit(3)


if __name__ == "__main__":
    main()
