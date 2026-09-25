#!/bin/bash
# =============================================
# xlite 定档位压测 — 对标 V88 喂法（**显式**传 KEEP_BODY=true + KEEP_SAMPLE=1）
#
# 与客户套件 `run_full_88.sh` 的关系：喂法对齐，默认值对齐；差别只有「一个 driver 打一
# 串档位」和「压完自动算账」。对标口径与三个指纹见 docs/manual/v88-rig.md §1 / §1.1。
#
# 喂法**显式传**（不靠 k6.js 的文件默认）：仓库版默认 KEEP_SAMPLE=1，客户机上那份 memopt
# 副本默认 0.1 —— 「不传」就是让「跑的是哪份文件」决定 checks 比。见 v88-rig.md §4.1。
#
# 这个脚本存在的理由：客户机上那份 `/root/loadtest/k6/k6.js` 在对标当晚被换过至少四次
# （v88-rig.md §1.1），只认路径必然会拿到别的版本。所以这里做两件事：
#   [1] preflight：打印 k6.js 的 md5 + 校验仓库版特征（KEEP_SAMPLE / GRACEFUL_STOP）
#   [2] 每档跑完由 report.py 断言 checks 比 2.00 与**期望的** gracefulStop（默认 10m0s，
#       随 GRACEFUL_STOP 一起透传），不符的档标 INVALID 且拒绝出报告
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
# shellcheck source=lib_rig.sh
. "$REPO_ROOT/scripts/loadtest/lib_rig.sh"
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
  # 采样窗结束才返回 —— k6 会在采样跑完之后才开打、`.acceptq` 记的是空载网关、每档白等
  # 一个采样窗（实测最小复现 2.17s vs 重定向后 0.01s）。
  # ⚠️ 而且**必须增量写盘**：driver 在 k6 结束时就把采样器 kill 掉（采样窗只是上界），
  # 只在循环结束后写一次的话，正常路径下文件永远不会生成 —— 证据静默消失。
  (
    max=0
    # 首个**有效**读数之前记 `unknown`：`0` 会被 report.py 当成「队列从未触顶」的真证据，
    # 而「采样器还没来得及读到就被 kill」或「ss 读不到行（端口未监听/无权限）」都会造成这种局面。
    printf 'unknown\n' > "$out"
    for _ in $(seq 1 "$n"); do
      v=$(ss -lnt "sport = :$port" 2>/dev/null | awk 'NR>1{print $2; exit}')
      case "${v:-}" in
        ''|*[!0-9]*) ;;                                   # 保持 unknown
        *) [ "$v" -gt "$max" ] && max="$v"; printf '%s\n' "$max" > "$out" ;;
      esac
      sleep 0.2
    done
    # `9>&-` 不可少：采样器是**后台子进程**，继承锁 fd 就会替 driver 继续持有台架锁。
    # 采样窗是 WARMUP+STABLE+60（默认 390s）⇒ driver 被 kill -9 时锁被攥住最长 6.5 分钟，
    # 与 lib_rig.sh / README / changelog 那句「kill -9 由内核释放、不留僵尸锁」直接矛盾。
  ) >/dev/null 2>&1 9>&- &
  printf '%s' "$!"
}

# ---------------------------------------------------------------- preflight
[ -f "$K6_SCRIPT" ] || { echo "✗ 找不到 K6_SCRIPT=$K6_SCRIPT" >&2; exit 3; }
grep -q 'var KEEP_SAMPLE' "$K6_SCRIPT" || { echo "✗ $K6_SCRIPT 不是仓库版（缺 KEEP_SAMPLE）" >&2; exit 3; }
grep -q 'gracefulStop: GRACEFUL_STOP' "$K6_SCRIPT" || { echo "✗ $K6_SCRIPT 不是仓库版（gracefulStop 未参数化）" >&2; exit 3; }
K6_SCRIPT_MD5=$(md5_of "$K6_SCRIPT")
mkdir -p "$RESULT_DIR"

# 台架互斥 —— 与 ab_configs.sh / ab_topology.sh 共用同一把锁（见 lib_rig.sh）
lab_lock "run_levels levels=$LEVELS" || exit 1

# k6 版本**只能这样拿**：`k6 run` 的 banner 不含版本行（不加 --verbose 时），所以从运行日志里
# grep 永远得到空值。docker 模式直接用镜像自报。
if [ "$K6_RUNNER" = local ]; then
  K6_VERSION=$(k6 version 2>/dev/null | awk '{print $2}' || true)
else
  K6_VERSION=$(docker run --rm "$K6_IMAGE" version 2>/dev/null | awk '{print $2}' || true)
fi
K6_VERSION=${K6_VERSION:-unknown}

log "preflight ok: $K6_SCRIPT md5=$K6_SCRIPT_MD5 k6=$K6_VERSION graceful_stop=$GRACEFUL_STOP"
# 打印**那份 k6.js 自己的** KEEP_SAMPLE 默认值：仓库版是 1，客户机上的 memopt 副本是 0.1。
# 我们总是显式传 `KEEP_SAMPLE=1`（见 run_k6），所以这个值**不影响**本脚本的喂法 —— 打出来
# 是为了让「文件与喂法不一致」这件事一眼可见，而不是靠默认值悄悄决定 checks 比。
log "k6.js 自带默认: $(grep -m1 -o 'var KEEP_SAMPLE = parseFloat(__ENV.KEEP_SAMPLE || .[0-9.]*.)' "$K6_SCRIPT" 2>/dev/null || echo '(未找到 var KEEP_SAMPLE)')（本脚本显式传 KEEP_SAMPLE=1）"
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
    GW_BACKLOG_SRC="explicit（同机实测=${DETECTED_BACKLOG:-读不到}）"
  elif [ -n "$DETECTED_BACKLOG" ]; then
    GATEWAY_BACKLOG="$DETECTED_BACKLOG"
    GW_BACKLOG_SRC="ss -lnt Send-Q（同机自动探测）"
  else
    GATEWAY_BACKLOG="unknown"
    GW_BACKLOG_SRC="unknown（本机没有 ss/读不到 Send-Q）"
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
    # KEEP_SAMPLE **显式传 1**（= 每次迭代全量解析 → checks 比 2.00）。不要靠文件默认：
    # 仓库版 k6.js 默认 1，但客户机上挂的 v3.2-memopt 副本默认 0.1 —— 「不传」等于让
    # **跑的是哪份文件**决定喂法，日志与实际就会不一致（踩坑记录见
    # docs/dev/loadtest-newapi-controlled-2026-09.md §13.6 第 1b 条）。
    -e "KEEP_SAMPLE=1"
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
    # 采样窗只是上界：k6 一结束就把采样器 kill 掉（采样器每 0.2s 增量落盘，见上）。
    # `9>&-`：采样器是后台子进程，继承锁 fd 就会替 driver 继续持有台架锁
    # （采样窗默认 30+300+60=390s ⇒ driver 被 kill -9 后锁被攥住 6.5 分钟）
    sampler_pid=$(start_accept_sampler "$GW_PORT" "$((WARMUP_SECS + STABLE_SECS + 60))" "$RESULT_DIR/$level.acceptq")
  fi
  run_k6 "$level" || log "level $level k6 退出码非 0（保留日志，交由 report.py 判定）"
  if [ -n "$sampler_pid" ]; then
    # 采样器是 `$( )` 里的子进程，主 shell 的 `wait` 拿不到它（实测恒返 127），所以只 kill；
    # 万一 kill 没命中，它也会在自己那个有界采样窗结束时自然退出，不会泄漏到下一档。
    kill "$sampler_pid" 2>/dev/null || true
  fi
  if [ -s "$RESULT_DIR/$level.acceptq" ]; then
    log "level $level accept 队列峰值 Recv-Q=$(cat "$RESULT_DIR/$level.acceptq") / backlog=$GATEWAY_BACKLOG"
  else
    log "⚠️ level $level 没有 .acceptq（采样器未落盘）—— accept 触顶无法判定"
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
