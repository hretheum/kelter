#!/bin/sh
# KRW.2b end to end, inside a throwaway clm:cpu (or clm:gpu) container:
#   encoder up → dataset → reference head evaluated → heads fine-tuned →
#   fine-tuned head evaluated. Everything written under /work/out.
# Mounts expected: /cache (weights), /work/clm-repo (CLM at a pinned commit),
# /work/train (this directory), /work/out (results).
set -eu
DEVICE="${CLM_DEVICE:-cpu}"
OUT=/work/out
mkdir -p "$OUT"
export CLM_API_KEY=train-local
pip install --quiet --no-cache-dir "pyarrow==21.0.0" >/dev/null

vllm serve Qwen/Qwen3-8B --served-model-name qwen3-8b --runner pooling \
  --enforce-eager --enable-prefix-caching --max-model-len 2048 --max-num-seqs 16 \
  --host 127.0.0.1 --port 8090 > "$OUT/encoder.log" 2>&1 &
wait_url() { # $1 url, $2 tries (5 s each)
  i=0
  until python3 -c "import urllib.request; urllib.request.urlopen('$1', timeout=3)" 2>/dev/null; do
    i=$((i + 1)); [ "$i" -gt "$2" ] && { echo "not up: $1" >&2; exit 1; }; sleep 5
  done
}
wait_url http://127.0.0.1:8090/v1/models 360

REF=$(python3 -c "from huggingface_hub import hf_hub_download as d; print(d('Contrastive-LM/CLM-v0.1-8B', 'CLM_v0.1-8B.pt'))")
echo "reference head: $REF"

python3 /work/train/build_dataset.py /work/out/data | tee "$OUT/dataset.txt"

serve() { # $1 port, $2 ckpt
  clm-serve --host 127.0.0.1 --port "$1" --emb-url http://127.0.0.1:8090/v1/embeddings \
    --emb-model qwen3-8b --max-tokens 2048 --device "$DEVICE" --no-ui --ckpt "$2" > "$OUT/serve-$1.log" 2>&1 &
  echo $!
}

P=$(serve 8700 "$REF"); wait_url http://127.0.0.1:8700/health 60
CLM_URL=http://127.0.0.1:8700 python3 /work/train/evaluate.py reference | tee "$OUT/eval-reference.txt"
kill "$P"

python3 /work/clm-repo/train/finetune.py --task choice --data /work/out/data --workflow path \
  --embed-url http://127.0.0.1:8090/v1/embeddings --served-model-name qwen3-8b \
  --init-ckpt "$REF" --targets hard --epochs 40 --patience 10 --val-frac 0.15 \
  --out-dir "$OUT/head" 2>&1 | tee "$OUT/finetune.log"

P=$(serve 8701 "$OUT/head/best_head.pt"); wait_url http://127.0.0.1:8701/health 60
CLM_URL=http://127.0.0.1:8701 python3 /work/train/evaluate.py finetuned | tee "$OUT/eval-finetuned.txt"
kill "$P"
echo DONE
