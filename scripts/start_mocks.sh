#!/bin/sh
set -eu
MODEL="${MOCK_MODEL:-mock}"
CHUNKS="${MOCK_CHUNKS:-20}"
INTERVAL="${MOCK_INTERVAL_MS:-10}"
TTFT="${MOCK_TTFT_MS:-0}"
LISTEN="${MOCK_LISTEN:-0.0.0.0}"

echo "[mocks] listen=${LISTEN} ports=4101-4124 model=${MODEL} chunks=${CHUNKS} interval_ms=${INTERVAL} ttft_ms=${TTFT}"
for port in $(seq 4101 4124); do
  node /app/stream_mock_server.mjs \
    --port "${port}" \
    --listen "${LISTEN}" \
    --model "${MODEL}" \
    --chunks "${CHUNKS}" \
    --interval-ms "${INTERVAL}" \
    --ttft-ms "${TTFT}" \
    --label "mock-${port}" &
done
wait
