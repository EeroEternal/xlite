#!/bin/bash
# =============================================
# xlite 定档位压测 — 对标 V88 喂法（KEEP_BODY=true，不传 KEEP_SAMPLE）
#
# 与客户套件 `run_full_88.sh` 的关系：喂法对齐，默认值对齐；差别只有「一个 driver 打一
# 串档位」和「压完自动算账」。对标口径与三个指纹见 docs/manual/v88-rig.md §1 / §1.1。
#
# 这个脚本存在的理由：客户机上那份 `/root/loadtest/k6/k6.js` 在对标当晚被换过至少四次
# （v88-rig.md §1.1），只认路径必然会拿到别的版本。所以这里做两件事：
#   [1] preflight：打印 k6.js 的 md5 + 校验仓库版特征（KEEP_SAMPLE / GRACEFUL_STOP）
#   [2] 每档跑完由 report.py 断言 checks 比 2.00 与 gracefulStop 10m0s，不符的档标
#       INVALID 且拒绝出报告
# 另外每档算 活跃流 = qps × dur_p50 并过一遍链路预算；越界就退出码 2、拒绝出报告。
#
# 环境变量（都有默认）:
#   TARGET=<url>          必填，如 http://10.99.255.101:18080/v1/chat/completions
#   TOKEN=<bearer>        必填
#   MODEL=mock-model      LAB 默认；实验室 YAML 里模型名可能是 mock
#   LEVELS="1000 4000 10000 31000"   档位（VU）
#   STABLE=300s WARMUP=30            每档稳态 / 爬坡
#   IN_TOKENS=100 OUT_TOKENS=500     与 B 组对齐
#   K6_RUNNER=docker|local           local 用本机 k6（调试用），docker 用 K6_IMAGE
#   K6_IMAGE=grafana/k6:latest       建议钉 digest：grafana/k6@sha256:…
#   K6_MEM=45g                       docker 内存护栏（KEEP_BODY 下 21k VU 约需 30G）
#   K6_SCRIPT=<path>                 默认仓库版 scripts/loadtest/k6/k6.js
#   RESULT_DIR=<dir>                 默认 scripts/loadtest/results/<UTC 时间戳>
#   MOCKS="8081-8104"                可选：跑前给 mock 下发 _config（端口区间或逗号分隔 host:port）
#   MOCK_TTFT=500 MOCK_INTERVAL=10   可选：_config 参数
#   LINK_MBPS=1000 | LINK_IFACE=eth0 可选：链路预算（不给就只算活跃流，不判越界）
#   GATEWAY_IMAGE=xlite:0.3.0        可选：写进 env.txt，报告里可追溯
#   ALLOW_LINKBOUND=1                可选：越界也出报告（仅诊断；对标报告不要开）
#
# 退出码：0 = 报告可用；2 = 有档 LINK-BOUND；3 = 指纹不符 / 无有效档位。
# =============================================

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
K6_SCRIPT=${K6_SCRIPT:-$REPO_ROOT/scripts/loadtest/k6/k6.js}
K6_RUNNER=${K6_RUNNER:-docker}
K6_IMAGE=${K6_IMAGE:-grafana/k6:latest}
K6_MEM=${K6_MEM:-45g}
TARGET=${TARGET:?需要 TARGET=<chat/completions url>}
TOKEN=${TOKEN:?需要 TOKEN=<bearer>}
MODEL=${MODEL:-mock-model}
IN_TOKENS=${IN_TOKENS:-100}
OUT_TOKENS=${OUT_TOKENS:-500}
LEVELS=${LEVELS:-"1000 4000 10000 31000"}
STABLE=${STABLE:-300s}
WARMUP=${WARMUP:-30}
RESULT_DIR=${RESULT_DIR:-$REPO_ROOT/scripts/loadtest/results/$(date -u +%Y%m%d-%H%M%S)}
MOCKS=${MOCKS:-}
MOCK_TTFT=${MOCK_TTFT:-500}
MOCK_INTERVAL=${MOCK_INTERVAL:-10}
LINK_MBPS=${LINK_MBPS:-}
LINK_IFACE=${LINK_IFACE:-}
GATEWAY_IMAGE=${GATEWAY_IMAGE:-unknown}
ALLOW_LINKBOUND=${ALLOW_LINKBOUND:-0}

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

md5_of() {
  if command -v md5sum >/dev/null 2>&1; then md5sum "$1" | awk '{print $1}';
  else md5 -q "$1"; fi
}

# ---------------------------------------------------------------- preflight
[ -f "$K6_SCRIPT" ] || { echo "✗ 找不到 K6_SCRIPT=$K6_SCRIPT" >&2; exit 3; }
grep -q 'var KEEP_SAMPLE' "$K6_SCRIPT" || { echo "✗ $K6_SCRIPT 不是仓库版（缺 KEEP_SAMPLE）" >&2; exit 3; }
grep -q 'gracefulStop: GRACEFUL_STOP' "$K6_SCRIPT" || { echo "✗ $K6_SCRIPT 不是仓库版（gracefulStop 未参数化）" >&2; exit 3; }
K6_SCRIPT_MD5=$(md5_of "$K6_SCRIPT")
mkdir -p "$RESULT_DIR"

log "preflight ok: $K6_SCRIPT md5=$K6_SCRIPT_MD5"
log "runner=$K6_RUNNER image=$K6_IMAGE target=$TARGET levels=[$LEVELS] stable=$STABLE out_tokens=$OUT_TOKENS"

# ---------------------------------------------------------------- mock _config（可选）
if [ -n "$MOCKS" ]; then
  case "$MOCKS" in
    [0-9]*-[0-9]*) hosts=$(seq "${MOCKS%-*}" "${MOCKS#*-}" | sed 's|^|127.0.0.1:|') ;;
    *) hosts=$(printf '%s' "$MOCKS" | tr ',' ' ') ;;
  esac
  for host in $hosts; do
    curl -s -m 3 -X POST "http://$host/_config?ttft=$MOCK_TTFT&token_interval=$MOCK_INTERVAL&out_tokens=$OUT_TOKENS&mix=0&mode=2&batch=1" >/dev/null 2>&1 || true
  done
  log "mock _config: ttft=$MOCK_TTFT interval=$MOCK_INTERVAL out_tokens=$OUT_TOKENS"
fi

# ---------------------------------------------------------------- run
run_k6() {
  local level="$1"
  local common=(
    -e "TARGET_URL=$TARGET" -e "TOKEN=$TOKEN" -e "MODEL=$MODEL"
    -e "STREAM=true" -e "IN_TOKENS=$IN_TOKENS" -e "OUT_TOKENS=$OUT_TOKENS"
    -e "VUS=$level" -e "MAX_VUS=$level" -e "DURATION=$STABLE" -e "WARMUP=$WARMUP"
    -e "STREAM_TIMEOUT=600000" -e "KEEP_BODY=true"
    # 不传 KEEP_SAMPLE（默认 1 = 每次迭代全量解析 → checks 比 2.00）
    # 不传 PICK_INTERVAL（默认 sleep(0)）、不传 XLITE_MAX_IN_FLIGHT
  )
  if [ "$K6_RUNNER" = local ]; then
    k6 run --compatibility-mode=base --summary-export="$RESULT_DIR/$level.summary.json" \
      "${common[@]}" "$K6_SCRIPT" > "$RESULT_DIR/$level.log" 2>&1
  else
    docker run --rm -m "$K6_MEM" --network=host \
      -v "$(dirname "$K6_SCRIPT"):/k6:ro" -v "$RESULT_DIR:/out" \
      "${common[@]}" \
      "$K6_IMAGE" run --compatibility-mode=base \
      --summary-export="/out/$level.summary.json" "/k6/$(basename "$K6_SCRIPT")" \
      > "$RESULT_DIR/$level.log" 2>&1
  fi
}

for level in $LEVELS; do
  log "level $level VU 开始"
  run_k6 "$level" || log "level $level k6 退出码非 0（保留日志，交由 report.py 判定）"
  log "level $level VU 结束（$RESULT_DIR/$level.log）"
done

# ---------------------------------------------------------------- env.txt（报告契约）
{
  echo "RUN_AT=$(date -u +%FT%TZ)"
  echo "REPO_HEAD=$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "GATEWAY_IMAGE=$GATEWAY_IMAGE"
  echo "K6_RUNNER=$K6_RUNNER"
  echo "K6_IMAGE=$K6_IMAGE"
  echo "K6_SCRIPT=$K6_SCRIPT"
  echo "K6_SCRIPT_MD5=$K6_SCRIPT_MD5"
  echo "TOKIO_WORKER_THREADS=${TOKIO_WORKER_THREADS:-unset（默认=核数）}"
  echo "TARGET=$TARGET"
  echo "MODEL=$MODEL"
  echo "IN_TOKENS=$IN_TOKENS"
  echo "OUT_TOKENS=$OUT_TOKENS"
  echo "LEVELS=$LEVELS"
  echo "STABLE=$STABLE"
  echo "WARMUP=$WARMUP"
  echo "SOMAXCONN=$(sysctl -n net.core.somaxconn 2>/dev/null || echo n/a)"
} > "$RESULT_DIR/env.txt"

# ---------------------------------------------------------------- 出报告（含链路预算闸门）
report_args=(--result-dir "$RESULT_DIR" --chunks "$((OUT_TOKENS + 2))")
[ -n "$LINK_MBPS" ] && report_args+=(--link-mbps "$LINK_MBPS")
[ -n "$LINK_IFACE" ] && report_args+=(--link-iface "$LINK_IFACE")
[ "$ALLOW_LINKBOUND" = "1" ] && report_args+=(--allow-linkbound)

log "report: python3 scripts/loadtest/report.py ${report_args[*]}"
python3 "$REPO_ROOT/scripts/loadtest/report.py" "${report_args[@]}"
