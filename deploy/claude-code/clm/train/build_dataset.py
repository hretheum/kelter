#!/usr/bin/env python3
"""Write tasks.py as the typed-decisions layout that CLM's
`train/finetune.py --task choice --workflow path` reads:
    <out>/path/train.parquet, <out>/path/test.parquet
Each row: id, workflow, state, questions, gold — the state and the question
exactly as the runner sends them (path_question.json is exported from the
runner, so training and serving cannot drift).

    python3 build_dataset.py <out-dir>
"""
import json
import os
import sys

import pyarrow as pa
import pyarrow.parquet as pq

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from tasks import TEST, TRAIN  # noqa: E402

QUESTION = json.load(open(os.path.join(HERE, "path_question.json")))
LABELS = set(QUESTION["criteria"])


def state(task, repo):
    # Mirrors runner/lib.mjs clmRequest(): {task, repository?}.
    return {"task": task, "repository": repo} if repo else {"task": task}


def rows(items, split):
    out = []
    for i, (label, repo, task) in enumerate(items):
        if label not in LABELS:
            raise SystemExit(f"{split}[{i}]: unknown label {label!r}")
        out.append({
            "id": f"{split}-{i:03d}",
            "workflow": "path",
            "state": json.dumps(state(task, repo), ensure_ascii=False),
            "questions": json.dumps({"sciezka": QUESTION}, ensure_ascii=False),
            "gold": json.dumps({"sciezka": {"label": label}}),
        })
    return out


def main():
    out = os.path.join(sys.argv[1] if len(sys.argv) > 1 else "data", "path")
    os.makedirs(out, exist_ok=True)
    seen = {t for _, _, t in TRAIN}
    leaked = [t for _, _, t in TEST if t in seen]
    if leaked:
        raise SystemExit(f"TEST tasks also in TRAIN: {leaked}")
    for split, items in (("train", TRAIN), ("test", TEST)):
        pq.write_table(pa.Table.from_pylist(rows(items, split)), os.path.join(out, f"{split}.parquet"))
        counts = {k: sum(1 for l, _, _ in items if l == k) for k in QUESTION["criteria"]}
        print(f"{split}: {len(items)} {counts}")


if __name__ == "__main__":
    main()
