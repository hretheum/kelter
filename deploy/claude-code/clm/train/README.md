---
provenance: ai_authored
provenance_metadata:
  agent: claude-opus-5-5
  session_date: 2026-10-13
  human_validated: false
  session_context: "Roadmap KRW.2b: fine-tuning CLM's heads for the path question, after the zero-shot reference head answered 'sonnet' for nearly everything (1/6 on hand-labelled tasks, KRW.2). Labels written by the agent from the rubric below; Eryk has not reviewed them yet."
---

# Fine-tuning CLM's heads for the path question (KRW.2b)

The reference CLM head, zero-shot, cannot tell the four paths apart (KRW.2,
BUILD-EVIDENCE 2026-10-13). This directory holds what it takes to train
heads that can: a labelled task set, the exact question the runner asks, and
the commands. The encoder stays frozen (Qwen3-8B, last-token pooling, as
served); only the two ~20M-parameter projection heads are trained — so a
new head is a new `--ckpt`, nothing else changes.

## Files

| file | what |
|---|---|
| `path_question.json` | the question **exactly** as the runner sends it (exported from `runner/lib.mjs`; a runner test fails if they drift) |
| `tasks.py` | the labelled tasks: `TRAIN` and a separately written `TEST`, English and Polish |
| `build_dataset.py` | → `data/path/train.parquet`, `data/path/test.parquet` in the typed-decisions layout `train/finetune.py --task choice` reads |
| `evaluate.py` | accuracy + confusion of a running `clm-serve` on `TEST` (before/after) |

## Labelling rubric

The label is the **cheapest path that will still succeed**, judged from the
task text alone (that is all CLM sees):

- **bez** — no repository change: summarize, list, explain, answer, write a
  note, plan. Reading code to answer a question is still `bez`.
- **haiku** — one mechanical change whose content is fully specified: a
  typo, a rename at one place, a config value, a version bump, a missing
  import, a one-line fix the task spells out.
- **sonnet** — ordinary engineering: a bug whose fix must be found, a test to
  write, a small feature or refactor across a few files, a CI or lint fix
  that needs diagnosis.
- **opus** — large or risky: core runtime semantics (exactly-once, resume,
  journaling, concurrency), security boundaries, data migrations, cross-cutting
  redesigns, or a task the brief says a cheaper model already failed.

Borderline cases lean to the more expensive label when a wrong cheap attempt
would be costly (core semantics), and to the cheaper one when a failed cheap
attempt is just retried.

## Running it (on K12, CPU encoder)

See the KRW.2b entry in `docs/BUILD-EVIDENCE.md` for the exact commands and
the memory guard used; in short: build the dataset, start the vLLM encoder
from the `clm:cpu` image with a small KV cache, run
`train/finetune.py --task choice --data <dir> --workflow path --embed-url …
--init-ckpt <reference head> --targets hard`, then serve the new head with
`CLM_CKPT` and run `evaluate.py`. Acceptance (roadmap): ≥ 80% on `TEST`.

## Results — `path-v1` (2026-10-13, K12, CPU)

| head | TEST (48) | bez | haiku | sonnet | opus |
|---|---|---|---|---|---|
| reference (`CLM_v0.1-8B.pt`, zero-shot) | **17/48 = 35%** | 2/12 | 0/12 | 12/12 | 3/12 |
| `path-v1` (fine-tuned, warm start) | **41/48 = 85%** | 8/12 | 12/12 | 12/12 | 9/12 |

Majority baseline 25%. Training: 102 questions after a 15% validation hold-out,
best epoch 11 of 21 (early stop), val acc 0.89, **1.3 min on the CPU**.
Errors left: `bez` read as `sonnet`/`opus` (4 — a costlier path suggested for
a no-code task) and `opus` read as `sonnet` (3 — the cheaper path; a failed
`sprawdz` escalates anyway). Raw output: `results/path-v1/`.

Caveats, stated plainly: 120 training tasks and 48 test tasks, both written
and labelled by the agent from the rubric (not yet reviewed by Eryk), so the
test measures generalisation across phrasings and subjects, not across
labellers. The next head should be trained on tasks from Kelter's own journal
(contract-verified runs + the cheapest path that passed).

The head (`path-v1.pt`, 75 MB, sha256 `4d2b4e27…d9`) is not in git; on K12
it is `~/cc-runner/clm-cache/heads/path-v1.pt` (in the container
`/cache/heads/path-v1.pt`), served with `CLM_CKPT=/cache/heads/path-v1.pt`.
Reproduce with `run_in_container.sh` (CLM code at `d5f9ef0`).
