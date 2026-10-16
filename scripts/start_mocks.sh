#!/bin/sh
set -eu
MODEL="${MOCK_MODEL:-mock}"
CHUNKS="${MOCK_CHUNKS:-20}"
INTERVAL="${MOCK_INTERVAL_MS:-10}"
TTFT="${MOCK_TTFT_MS:-0}"
# 每 N 个 chunk 合并成一次 write（不改载荷字节）。延迟测试用 1，极限吞吐测试可用 10。
BATCH="${MOCK_BATCH:-1}"
LISTEN="${MOCK_LISTEN:-0.0.0.0}"

echo "[mocks] listen=${LISTEN} ports=4101-4124 model=${MODEL} chunks=${CHUNKS} interval_ms=${INTERVAL} ttft_ms=${TTFT} batch=${BATCH}"
for port in $(seq 4101 4124); do
  node /app/stream_mock_server.mjs \
    --port "${port}" \
    --listen "${LISTEN}" \
    --model "${MODEL}" \
    --chunks "${CHUNKS}" \
    --interval-ms "${INTERVAL}" \
    --ttft-ms "${TTFT}" \
    --batch "${BATCH}" \
    --label "mock-${port}" &
done
wait
