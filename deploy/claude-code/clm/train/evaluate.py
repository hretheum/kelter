#!/usr/bin/env python3
"""Accuracy and confusion of a running clm-serve on tasks.TEST — asked
exactly as the runner asks (same state shape, same question).

    CLM_URL=http://127.0.0.1:8700 CLM_API_KEY=... python3 evaluate.py [label]
Prints one JSON line (machine-readable) after the human-readable table.
"""
import json
import os
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from tasks import TEST  # noqa: E402

QUESTION = json.load(open(os.path.join(HERE, "path_question.json")))
KEYS = list(QUESTION["criteria"])
URL = os.environ.get("CLM_URL", "http://127.0.0.1:8700").rstrip("/")
KEY = os.environ.get("CLM_API_KEY", "")


def ask(task, repo):
    st = {"task": task, "repository": repo} if repo else {"task": task}
    body = json.dumps({"model": "clm-latest", "state": st, "questions": {"sciezka": QUESTION}}).encode()
    req = urllib.request.Request(f"{URL}/v1/systemone", body,
                                 {"content-type": "application/json", "authorization": f"Bearer {KEY}"})
    return json.load(urllib.request.urlopen(req, timeout=120))["answers"]["sciezka"]


def main():
    name = sys.argv[1] if len(sys.argv) > 1 else "head"
    conf = {w: {g: 0 for g in KEYS} for w in KEYS}
    ok = 0
    t0 = time.time()
    for want, repo, task in TEST:
        got = ask(task, repo)["choice"]
        conf[want][got] += 1
        ok += want == got
    n = len(TEST)
    print(f"{name}: {ok}/{n} = {ok / n:.0%}  ({(time.time() - t0) / n:.2f} s/question)")
    print("want \\ got " + " ".join(f"{k:>7s}" for k in KEYS))
    for w in KEYS:
        print(f"{w:>10s} " + " ".join(f"{conf[w][g]:7d}" for g in KEYS))
    print(json.dumps({"head": name, "correct": ok, "total": n, "accuracy": ok / n, "confusion": conf}))


if __name__ == "__main__":
    main()
