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
# 另外每档算 活跃流 = VUS × dur/iter（闭式；`qps × dur` 会被排空段稀释，只作对照列）
# 并过一遍链路预算；越界就退出码 2、拒绝出报告。
# 同机跑时还会采样网关 accept 队列峰值（ss 的 Recv-Q），逼近 backlog 即判触顶。
#
# 环境变量（都有默认）:
#   TARGET=<url>          必填，如 http://10.99.255.101:18080/v1/chat/completions
#   TOKEN=<bearer>        必填
#   MODEL=mock-model      LAB 默认；实验室 YAML 里模型名可能是 mock
#   LEVELS="1000 4000 10000 31000"   档位（VU）
#   STABLE=300s WARMUP=30            每档稳态 / 爬坡
#   GRACEFUL_STOP=600s               透传给 k6 的收尾时间。**改了它，指纹期望值也要跟着改**：
#                                    本脚本把同一个值传给 report.py --expect-graceful-stop，
#                                    所以 `GRACEFUL_STOP=30s` 不会被误判成「脚本被换过」。
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
#   GATEWAY_BACKLOG=<n>              可选：网关监听套接字的 backlog。**跨机时必填** ——
#                                    它与内核 net.core.somaxconn 不是一回事（libc SOMAXCONN
#                                    默认 128，会把 backlog 压到 128）。同机时本脚本自动读
#                                    `ss -lnt` 的 Send-Q，并额外采样 accept 队列峰值。
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
GRACEFUL_STOP=${GRACEFUL_STOP:-600s}
RESULT_DIR=${RESULT_DIR:-$REPO_ROOT/scripts/loadtest/results/$(date -u +%Y%m%d-%H%M%S)}
MOCKS=${MOCKS:-}
MOCK_TTFT=${MOCK_TTFT:-500}
MOCK_INTERVAL=${MOCK_INTERVAL:-10}
LINK_MBPS=${LINK_MBPS:-}
LINK_IFACE=${LINK_IFACE:-}
GATEWAY_IMAGE=${GATEWAY_IMAGE:-unknown}
GATEWAY_BACKLOG=${GATEWAY_BACKLOG:-}
ALLOW_LINKBOUND=${ALLOW_LINKBOUND:-0}

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }

md5_of() {
  if command -v md5sum >/dev/null 2>&1; then md5sum "$1" | awk '{print $1}';
  else md5 -q "$1"; fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# ------------------------------------------------------- 网关监听套接字观测
# ⚠️ 网关的 backlog 与内核 `net.core.somaxconn` **不是一回事**：libc `SOMAXCONN` 默认 128，
# `TcpListener::bind` 用的是它，会把 backlog 压到 128（`somaxconn` 再大也没用）。
# 所以报告里必须记 **监听套接字自己的 Send-Q**，而不是 somaxconn。见 §11.6。
gateway_hostport() { printf '%s' "$TARGET" | sed -E 's#^[a-z]+://##; s#/.*$##'; }

# 目标是本机时返回端口号；跨机返回空（跨机请显式传 GATEWAY_BACKLOG）。
local_port_of() {
  local hp="$1" host port
  host="${hp%:*}"; port="${hp##*:}"
  [ "$port" != "$hp" ] || return 1
  case "$host" in
    127.0.0.1|localhost|0.0.0.0|::1|'[::1]') printf '%s' "$port" ;;
    *) hostname -I 2>/dev/null | tr ' ' '\n' | grep -qxF "$host" && printf '%s' "$port" || return 1 ;;
  esac
}

# LISTEN 套接字的 backlog 上限（ss 的 Send-Q 列）。
listen_backlog_of() { ss -lnt "sport = :$1" 2>/dev/null | awk 'NR>1{print $3; exit}'; }

# 采样 accept 队列深度峰值（ss 的 Recv-Q 列）。**这是「backlog 到底触不触顶」的直接证据**：
# 峰值逼近 backlog 就是触顶，与「负载高低」无关。$1=端口 $2=秒数 $3=输出文件
start_accept_sampler() {
  local port="$1" secs="$2" out="$3" n
  # `ss` 是 Linux 工具。没有它（macOS 开发机）就记 `unknown`，**绝不能记 0** ——
  # 0 会被读成「队列从未触顶」，那是假证据。
  if ! command -v ss >/dev/null 2>&1; then
    printf 'unknown\n' > "$out"
    return 0
  fi
  n=$(( secs * 5 ))
  # ⚠️ 后台子 shell **必须重定向 stdout**：否则它继承 `$( )` 的管道，命令替换要等整个
  # 采样窗结束才返回 —— k6 会在采样跑完之后才开打，`.acceptq` 记的是空载网关，每档白等
  # 一个采样窗（实测最小复现 2.17s vs 重定向后 0.01s）。
  (
    max=0
    for _ in $(seq 1 "$n"); do
      v=$(ss -lnt "sport = :$port" 2>/dev/null | awk 'NR>1{print $2; exit}')
      case "${v:-}" in ''|*[!0-9]*) ;; *) [ "$v" -gt "$max" ] && max="$v" ;; esac
      sleep 0.2
    done
    printf '%s\n' "$max" > "$out"
  ) >/dev/null 2>&1 &
  printf '%s' "$!"
}

# ---------------------------------------------------------------- preflight
[ -f "$K6_SCRIPT" ] || { echo "✗ 找不到 K6_SCRIPT=$K6_SCRIPT" >&2; exit 3; }
grep -q 'var KEEP_SAMPLE' "$K6_SCRIPT" || { echo "✗ $K6_SCRIPT 不是仓库版（缺 KEEP_SAMPLE）" >&2; exit 3; }
grep -q 'gracefulStop: GRACEFUL_STOP' "$K6_SCRIPT" || { echo "✗ $K6_SCRIPT 不是仓库版（gracefulStop 未参数化）" >&2; exit 3; }
K6_SCRIPT_MD5=$(md5_of "$K6_SCRIPT")
mkdir -p "$RESULT_DIR"

# k6 版本**只能这样拿**：`k6 run` 的 banner 不含版本行（不加 --verbose 时），所以从运行日志里
# grep 永远得到空值。docker 模式直接用镜像自报。
if [ "$K6_RUNNER" = local ]; then
  K6_VERSION=$(k6 version 2>/dev/null | awk '{print $2}' || true)
else
  K6_VERSION=$(docker run --rm "$K6_IMAGE" version 2>/dev/null | awk '{print $2}' || true)
fi
K6_VERSION=${K6_VERSION:-unknown}

log "preflight ok: $K6_SCRIPT md5=$K6_SCRIPT_MD5 k6=$K6_VERSION graceful_stop=$GRACEFUL_STOP"
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

# ---------------------------------------------------------------- 网关套接字（同机时才拿得到）
GW_HP=$(gateway_hostport)
GW_PORT=$(local_port_of "$GW_HP" || true)
GW_BACKLOG_SRC=""
if [ -n "$GW_PORT" ]; then
  DETECTED_BACKLOG=$(listen_backlog_of "$GW_PORT" || true)
  if [ -n "${GATEWAY_BACKLOG:-}" ]; then
    GW_BACKLOG_SRC="explicit（同机实测=${DETECTED_BACKLOG}）"
  else
    GATEWAY_BACKLOG="$DETECTED_BACKLOG"
    GW_BACKLOG_SRC="ss -lnt Send-Q（同机自动探测）"
  fi
  log "网关 $GW_HP 本机可观测：backlog=$GATEWAY_BACKLOG 来源=$GW_BACKLOG_SRC"
else
  if [ -n "${GATEWAY_BACKLOG:-}" ]; then
    GW_BACKLOG_SRC="explicit（跨机，网关不在本机）"
  else
    GATEWAY_BACKLOG="unknown"
    GW_BACKLOG_SRC="unknown（网关不在本机，且未传 GATEWAY_BACKLOG）"
    log "⚠️ 网关 $GW_HP 不在本机且未传 GATEWAY_BACKLOG —— 无法测 backlog，报告里记为 unknown"
  fi
fi
STABLE_SECS=${STABLE%s}; WARMUP_SECS=${WARMUP%s}

# ---------------------------------------------------------------- run
run_k6() {
  local level="$1"
  local common=(
    -e "TARGET_URL=$TARGET" -e "TOKEN=$TOKEN" -e "MODEL=$MODEL"
    -e "STREAM=true" -e "IN_TOKENS=$IN_TOKENS" -e "OUT_TOKENS=$OUT_TOKENS"
    -e "VUS=$level" -e "MAX_VUS=$level" -e "DURATION=$STABLE" -e "WARMUP=$WARMUP"
    -e "STREAM_TIMEOUT=600000" -e "KEEP_BODY=true" -e "GRACEFUL_STOP=$GRACEFUL_STOP"
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
  sampler_pid=""
  if [ -n "$GW_PORT" ]; then
    sampler_pid=$(start_accept_sampler "$GW_PORT" "$((WARMUP_SECS + STABLE_SECS + 60))" "$RESULT_DIR/$level.acceptq")
  fi
  run_k6 "$level" || log "level $level k6 退出码非 0（保留日志，交由 report.py 判定）"
  if [ -n "$sampler_pid" ]; then kill "$sampler_pid" 2>/dev/null || true; fi
  if [ -s "$RESULT_DIR/$level.acceptq" ]; then
    log "level $level accept 队列峰值 Recv-Q=$(cat "$RESULT_DIR/$level.acceptq") / backlog=$GATEWAY_BACKLOG"
  fi
  log "level $level VU 结束（$RESULT_DIR/$level.log）"
done

# ---------------------------------------------------------------- env.txt（报告契约）
{
  echo "RUN_AT=$(date -u +%FT%TZ)"
  echo "REPO_HEAD=$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "GATEWAY_IMAGE=$GATEWAY_IMAGE"
  echo "K6_RUNNER=$K6_RUNNER"
  echo "K6_IMAGE=$K6_IMAGE"
  echo "K6_IMAGE_ID=$(docker image inspect --format '{{.Id}}' "$K6_IMAGE" 2>/dev/null || echo n/a)"
  echo "K6_VERSION=$K6_VERSION"
  echo "K6_SCRIPT=$K6_SCRIPT"
  echo "K6_SCRIPT_MD5=$K6_SCRIPT_MD5"
  echo "K6_SCRIPT_SHA256=$(sha256_of "$K6_SCRIPT")"
  echo "TOKIO_WORKER_THREADS=${TOKIO_WORKER_THREADS:-unset（默认=核数）}"
  echo "XLITE_CATALOG_RELOAD_SECS=${XLITE_CATALOG_RELOAD_SECS:-unset（网关侧默认 5s）}"
  echo "GATEWAY_BACKLOG=$GATEWAY_BACKLOG"
  echo "GATEWAY_BACKLOG_SOURCE=$GW_BACKLOG_SRC"
  echo "SOMAXCONN=$(sysctl -n net.core.somaxconn 2>/dev/null || echo n/a)"
  echo "TARGET=$TARGET"
  echo "MODEL=$MODEL"
  echo "IN_TOKENS=$IN_TOKENS"
  echo "OUT_TOKENS=$OUT_TOKENS"
  echo "LEVELS=$LEVELS"
  echo "STABLE=$STABLE"
  echo "WARMUP=$WARMUP"
} > "$RESULT_DIR/env.txt"

# ---------------------------------------------------------------- 出报告（含链路预算闸门）
report_args=(--result-dir "$RESULT_DIR" --expect-graceful-stop "$GRACEFUL_STOP")
[ -n "$LINK_MBPS" ] && report_args+=(--link-mbps "$LINK_MBPS")
[ -n "$LINK_IFACE" ] && report_args+=(--link-iface "$LINK_IFACE")
[ "$ALLOW_LINKBOUND" = "1" ] && report_args+=(--allow-linkbound)

log "report: python3 scripts/loadtest/report.py ${report_args[*]}"
python3 "$REPO_ROOT/scripts/loadtest/report.py" "${report_args[@]}"
