#!/bin/bash
# =============================================
# 端到端：证明「reload → 换快照 → 读者看到新快照」，以及并发下没有撕裂/失败。
#
# 为什么需要它：A/B harness（`ab_configs.sh`）量的是**同配置下的 CPU/延迟差异**，而目录冻结
# 时 `reload()` 会在内容不变处提前 return —— 也就是说它**根本没走到**被改的换快照路径
# （见 `docs/dev/loadtest-newapi-controlled-2026-09.md` §12.6）。这个脚本反过来：**制造一次
# 真实的路由变化**，然后用「旧模型 404 / 新模型 200」直接证明换代可见，并可选地在换快照
# 窗口内压一把，检查 fail=0。
#
# 用法：
#   XLITE_BIN=/path/to/xlite scripts/loadtest/e2e_reload_swap.sh
#   XLITE_BIN=… VUS=20 DURATION=10s scripts/loadtest/e2e_reload_swap.sh   # 带并发段
#
# 环境变量：
#   XLITE_BIN     必填（或自动找 target/debug/xlite）
#   MOCK_PORT=4195 GW_PORT=18120 OUT=<tmp 目录>
#   RELOAD_SECS=1  网关目录重载周期（要 ≤3，否则换快照窗口太长）
#   VUS=0          0 = 跳过并发段（只验换快照）
#   DURATION=10s   并发段时长
#
# 退出码：0 = 全部断言通过；非 0 = 有断言失败（并打印哪一条）。
# =============================================
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
XLITE_BIN=${XLITE_BIN:-$REPO_ROOT/target/debug/xlite}
MOCK_PORT=${MOCK_PORT:-4195}
GW_PORT=${GW_PORT:-18120}
RELOAD_SECS=${RELOAD_SECS:-1}
VUS=${VUS:-0}
DURATION=${DURATION:-10s}
OUT=${OUT:-$(mktemp -d /tmp/xlite-e2e-reload.XXXXXX)}

[ -x "$XLITE_BIN" ] || { echo "✗ 找不到可执行文件 XLITE_BIN=${XLITE_BIN}（先 cargo build）" >&2; exit 2; }
command -v node >/dev/null || { echo "✗ 需要 node 跑 mock（scripts/stream_mock_server.mjs）" >&2; exit 2; }
mkdir -p "$OUT"

MOCK_PID=""; GW_PID=""
cleanup() {
  [ -n "$GW_PID" ] && kill "$GW_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

cat_yaml() { # $1=模型名
  cat <<YAML
providers:
  - name: mock-a
    base_url: http://127.0.0.1:$MOCK_PORT
    api_key: ""
    enabled: true
models:
  - name: $1
    enabled: true
    channels:
      - provider: mock-a
        upstream_model: mock
        weight: 1
        priority: 0
        enabled: true
api_keys:
  - name: loadtest
    key: sk-loadtest
    models: []
    enabled: true
YAML
}

cat_yaml mock  > "$OUT/catalog-a.yaml"
cat_yaml mock2 > "$OUT/catalog-b.yaml"
DB="sqlite:$OUT/e2e.db"

echo "[e2e] mock :$MOCK_PORT"
node "$REPO_ROOT/scripts/stream_mock_server.mjs" --port "$MOCK_PORT" --listen 127.0.0.1 \
  --model mock --chunks 5 --interval-ms 10 --ttft-ms 50 > "$OUT/mock.log" 2>&1 &
MOCK_PID=$!
for _ in $(seq 1 50); do curl -s -m 1 "http://127.0.0.1:$MOCK_PORT/health" >/dev/null 2>&1 && break; sleep 0.2; done

echo "[e2e] import catalog A（model=mock）"
DATABASE_URL="$DB" "$XLITE_BIN" import "$OUT/catalog-a.yaml" > "$OUT/import-a.log" 2>&1

echo "[e2e] 起网关 :${GW_PORT}（XLITE_CATALOG_RELOAD_SECS=${RELOAD_SECS}）"
DATABASE_URL="$DB" PORT="$GW_PORT" LISTEN_HOST=127.0.0.1 \
  XLITE_CATALOG_RELOAD_SECS="$RELOAD_SECS" "$XLITE_BIN" > "$OUT/gateway.log" 2>&1 &
GW_PID=$!
for _ in $(seq 1 80); do curl -s -m 1 "http://127.0.0.1:$GW_PORT/health" >/dev/null 2>&1 && break; sleep 0.25; done

code() { # $1=模型名 → HTTP 状态码
  curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$GW_PORT/v1/chat/completions" \
    -H 'Authorization: Bearer sk-loadtest' -H 'Content-Type: application/json' \
    -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"stream\":true}"
}

fail=0
check() { # $1=描述 $2=实际 $3=期望
  if [ "$2" = "$3" ]; then echo "  ✓ $1 = $2"; else echo "  ✗ $1 = $2（期望 $3）" >&2; fail=1; fi
}

echo "[e2e] 换快照前"
check "mock(旧模型)  " "$(code mock)"  200
check "mock2(新模型) " "$(code mock2)" 404

echo "[e2e] import catalog B（model=mock2）→ 等 $((RELOAD_SECS * 2))s 让 reload 循环跑两轮"
DATABASE_URL="$DB" "$XLITE_BIN" import "$OUT/catalog-b.yaml" --replace > "$OUT/import-b.log" 2>&1
sleep $((RELOAD_SECS * 2))

echo "[e2e] 换快照后（同一进程，未重启）"
check "mock(旧模型)  " "$(code mock)"  404
check "mock2(新模型) " "$(code mock2)" 200

if [ "$VUS" -gt 0 ]; then
  command -v k6 >/dev/null || { echo "✗ VUS>0 但本机没有 k6" >&2; exit 2; }
  echo "[e2e] 并发段：${VUS} VU × ${DURATION}（换快照窗口内）"
  k6 run --compatibility-mode=base --summary-export="$OUT/k6.json" \
    -e TARGET_URL="http://127.0.0.1:$GW_PORT/v1/chat/completions" -e TOKEN=sk-loadtest -e MODEL=mock2 \
    -e STREAM=true -e IN_TOKENS=4 -e OUT_TOKENS=5 -e VUS="$VUS" -e MAX_VUS="$VUS" -e WARMUP=0 \
    -e DURATION="${DURATION}" -e STREAM_TIMEOUT=30000 -e KEEP_BODY=true \
    "$REPO_ROOT/scripts/loadtest/k6/k6.js" > "$OUT/k6.log" 2>&1 || true
  python3 - "$OUT/k6.json" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))["metrics"]
fails = m["http_req_failed"]["value"] * 100
print(f"  {'✓' if fails == 0 else '✗'} 并发段 fail = {fails:.2f}%   http_reqs = {m['http_reqs']['count']:.0f}")
sys.exit(0 if fails == 0 else 1)
PY
  [ $? -eq 0 ] || fail=1
fi

[ "$fail" -eq 0 ] && { echo "[e2e] ✓ 全部通过，产物在 $OUT"; exit 0; }
echo "[e2e] ✗ 有断言失败，产物在 $OUT" >&2
exit 1
