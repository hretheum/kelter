#!/usr/bin/env python3
# Scripted OpenAI-compatible /v1/chat/completions: replies from a queue,
# then `finish` forever. Logs every reply it served to calls.log (cwd), and
# every request it received (the prompt the model saw) to requests.log.
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

QUEUE = [
    # run R (rejections): schema-invalid, valid append, duplicate, finish
    {"action": "append_ledger", "arguments": {"entry": 5}},
    {"action": "append_ledger", "arguments": {"entry": "journal-hello"}},
    {"action": "append_ledger", "arguments": {"entry": "journal-hello"}},
    {"action": "finish", "arguments": {"summary": "appended once"}},
    # run K (killed after the side effect): append; after restart, finish
    {"action": "append_ledger", "arguments": {"entry": "durable-hello"}},
]
FINISH = {"action": "finish", "arguments": {"summary": "done"}}
# Another smoke can supply its own script: FAKE_LLM_QUEUE=<path to a JSON list>.
if os.environ.get("FAKE_LLM_QUEUE"):
    with open(os.environ["FAKE_LLM_QUEUE"]) as f:
        QUEUE = json.load(f)

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with open("requests.log", "a") as f:
            f.write(json.dumps(body) + "\n")
        reply = QUEUE.pop(0) if QUEUE else FINISH
        with open("calls.log", "a") as f:
            f.write(json.dumps(reply) + "\n")
        out = {"choices": [{"message": {"role": "assistant", "content": json.dumps(reply)},
                            "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 10, "completion_tokens": 5}}
        data = json.dumps(out).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers(); self.wfile.write(data)

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
