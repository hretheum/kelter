#!/bin/sh
# Start the Qwen3-8B pooling encoder, wait until it serves, then run clm-serve
# in front of it. The settings follow the reference (serve_qwen3_8b.sh in the
# CLM repo): last-token pooling + prefix cache, short max length — the head
# only matches embeddings produced exactly this way.
#   CLM_DEVICE       cuda | cpu   (set by the profile drop-in)
#   CLM_GPU_UTIL     share of GPU memory for the encoder (GPU profile)
#   CLM_API_KEY_FILE bearer key file for clm-serve (mounted secret)
set -eu
DEVICE="${CLM_DEVICE:-cuda}"
EXTRA=""
if [ "$DEVICE" = "cuda" ]; then
  EXTRA="--gpu-memory-utilization ${CLM_GPU_UTIL:-0.65}"
fi
# shellcheck disable=SC2086
vllm serve Qwen/Qwen3-8B \
  --served-model-name qwen3-8b \
  --runner pooling \
  --enforce-eager \
  --enable-prefix-caching \
  --max-model-len 2048 \
  --max-num-seqs 16 \
  --host 127.0.0.1 --port 8090 $EXTRA &
ENC=$!
# First start downloads ~16 GB of weights: wait up to an hour.
i=0
until python3 -c "import urllib.request,sys; urllib.request.urlopen('http://127.0.0.1:8090/v1/models', timeout=3)" 2>/dev/null; do
  kill -0 "$ENC" 2>/dev/null || { echo "encoder exited" >&2; exit 1; }
  i=$((i + 1)); [ "$i" -gt 720 ] && { echo "encoder not ready after 60 min" >&2; exit 1; }
  sleep 5
done
CLM_API_KEY="$(cat "${CLM_API_KEY_FILE:-/etc/clm/api-key}")"
export CLM_API_KEY
exec clm-serve --host 0.0.0.0 --port 8700 --emb-url http://127.0.0.1:8090/v1/embeddings \
  --emb-model qwen3-8b --max-tokens 2048 --device "$DEVICE" --no-ui
