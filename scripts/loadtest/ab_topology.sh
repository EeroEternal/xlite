#!/bin/bash
# =============================================
# ab_topology.sh —— 用「同机 loopback / 拆分拓扑」找**瓶颈落在哪一组进程**。
#
# 与另外两个脚本的分工：
#   run_levels.sh  = 对标 New API 的定档位压测（要 gw_oh，带报告闸门）
#   ab_configs.sh  = 判某个**旋钮/改动**值不值（判决指标含网关 CPU）
#   ab_topology.sh = 判**瓶颈在谁身上**：网关 / mock 农场 / k6 各吃多少核
#
# 为什么需要它：链路受限的台架上，1 GbE 先撞顶（实测拐点 ≈990 req/s ≈100 MB/s），
# 什么都测不出。把三组进程挪到同机 loopback 就能把链路从路径上拿掉，
# 然后**分别**采三组进程的 CPU，看谁先饱和。
#
# 已测结论（1000→16000 VU，见 docs/dev/loadtest-newapi-controlled-2026-09.md §13）：
#   **农场才是瓶颈，不是网关。** 农场 ≈34–38 核/千请求/秒，网关 ≈7，k6 ≈12。
#   16000 VU 同机时：农场 72 核 / 网关 9.5 核 / k6 16–18 核（全机 128 核）。
#
# 用法:
#   ARM=A2 TOPOLOGY=local KEEP_BODY=false LEVELS="1000 4000 8000" scripts/loadtest/ab_topology.sh
#   ARM=A2 TOPOLOGY=split REMOTE_HOST=10.99.255.101 LEVELS="4000 8000" scripts/loadtest/ab_topology.sh
#
# 环境变量（都有默认）:
#   ARM=A2                    用 GATEWAY_DIR/xlite-<ARM> 这个二进制
#   PORT=18120
#   TOPOLOGY=local            见下
#   REMOTE_HOST=10.99.255.101 TOPOLOGY=split 时 k6 跑在这台（跨链路）
#   KEEP_BODY=false           ★压瓶颈模式用 false：true 是对标模式，k6 自己会先饱和
#                             （实测 true 时 qps 在 4000 VU 就封顶 ~313，白丢 4 倍吞吐）
#   TWT=8 CONNS=32 RELOAD=0   → TOKIO_WORKER_THREADS / XLITE_UPSTREAM_MAX_CONNS_PER_HOST
#                             / XLITE_CATALOG_RELOAD_SECS（0=冻结快照，去掉 reload 噪声）
#   LEVELS="1000 4000 8000 16000 31000"  DURATION=120s WARMUP=20 GRACEFUL_STOP=20s
#   MOCK_PORTS="4101-4124"    农场端口区间
#   LAB_LOCK=$HOME/lab.lock   台架互斥锁（见下）
#
# 两种拓扑:
#   local  k6 ─┐
#              ├─ 全部在本机（loopback，链路不在路径上）
#   网关 ─ mock ┘
#
#   split  k6 在 REMOTE_HOST ──(1 GbE)──▶ 网关 ─ mock（后两者在本机 loopback）
#          链路只承担「网关→k6」一程，容量口径上比原台架翻倍
#
# ★ 台架互斥（必须）: 两个 session 同时压测，量出来的是「谁在抢资源」的函数。
#   脚本开头抢锁，抢不到直接退出。锁绑在 fd 上，进程一死内核自动释放 ⇒ 无僵尸锁。
#   split 拓扑会**同时**在 REMOTE_HOST 上抢一把，避免只锁住本机。
#   驱动压测请固定用一台机（本约定：用网关+mock 所在的那台）。
#
# =============================================

set -uo pipefail

GATEWAY_DIR=${GATEWAY_DIR:-$HOME/ab6}
ARM=${ARM:-A2}
PORT=${PORT:-18120}
TOPOLOGY=${TOPOLOGY:-local}
REMOTE_HOST=${REMOTE_HOST:-10.99.255.101}
KEEP_BODY=${KEEP_BODY:-false}
TWT=${TWT:-8}
CONNS=${CONNS:-32}
RELOAD=${RELOAD:-0}
LEVELS=${LEVELS:-"1000 4000 8000 16000 31000"}
DURATION=${DURATION:-120s}
WARMUP=${WARMUP:-20}
GRACEFUL_STOP=${GRACEFUL_STOP:-20s}
MOCK_PORTS=${MOCK_PORTS:-"4101-4124"}
MOCK_CONTAINER_PREFIX=${MOCK_CONTAINER_PREFIX:-mock-h2c-}
K6_DIR=${K6_DIR:-$HOME/labtest/k6dir2}
K6_IMAGE=${K6_IMAGE:-k6-customer:latest}
K6_MEM=${K6_MEM:-45g}
TOKEN=${TOKEN:-sk-loadtest}
MODEL=${MODEL:-mock}
REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
# 目录文件必须指向 127.0.0.1（同机 loopback）。仓库里的 examples/mock-24.yaml 就是这份；
# 台架上没有 checkout 时退回 $HOME/labtest/mock-24.yaml。
CATALOG=${CATALOG:-}
if [ -z "$CATALOG" ]; then
  for c in "$REPO_ROOT/examples/mock-24.yaml" "$HOME/labtest/mock-24.yaml"; do
    [ -s "$c" ] && { CATALOG=$c; break; }
  done
fi
OUT=${OUT:-$HOME/labtest/ab-topology}
LAB_LOCK=${LAB_LOCK:-$HOME/lab.lock}
LAB_SESSION=${LAB_SESSION:-"$(hostname)-$$"}
SSH_OPTS=${SSH_OPTS:--o BatchMode=yes -o StrictHostKeyChecking=no}

mkdir -p "$OUT"
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/progress.log"; }
die() { echo "✗ $*" >&2; exit 1; }

# ---------------------------------------------------------------- 台架互斥
# 锁绑 fd 9，脚本退出/被杀都由内核释放 —— 不会留僵尸锁。
take_local_lock() {
  exec 9>"$LAB_LOCK"
  if ! flock -n 9; then
    echo "✗ 台架被占用：$(cat "$LAB_LOCK" 2>/dev/null)" >&2
    echo "  确认无人使用后再删锁文件：rm $LAB_LOCK" >&2
    exit 1
  fi
  echo "$LAB_SESSION $(date -Is) $*" > "$LAB_LOCK"
}

REMOTE_LOCK_PID=""
take_remote_lock() {
  [ "$TOPOLOGY" = split ] || return 0
  local first
  # 远端持锁 + 长睡，直到我们 kill 这个 ssh（fd 关闭 ⇒ 远端锁释放）
  coproc RLOCK { ssh $SSH_OPTS "$REMOTE_HOST" \
    "exec 9>~/.lab.lock; flock -n 9 || { echo BUSY; exit 1; }; echo OK; sleep 86400"; }
  REMOTE_LOCK_PID=$RLOCK_PID
  read -r -t 20 first <&"${RLOCK[0]}" || true
  [ "$first" = "OK" ] || die "远端 $REMOTE_HOST 台架被占用（$first）"
  ssh $SSH_OPTS "$REMOTE_HOST" "echo '$LAB_SESSION $(date -Is) topology-split' > ~/.lab.lock" 2>/dev/null || true
}

# 端口区间 → 空格分隔
expand_ports() {
  case "$1" in
    [0-9]*-[0-9]*) seq "${1%-*}" "${1#*-}" ;;
    *) printf '%s' "$1" | tr ',' ' ' ;;
  esac
}

cleanup() {
  local pid
  pid=$(ss -lntp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
  [ -n "$pid" ] && kill "$pid" 2>/dev/null
  pkill -f "ab_topology.*sampler" 2>/dev/null
  docker rm -f "abtop-k6-$PORT" >/dev/null 2>&1
  [ "$TOPOLOGY" = split ] && ssh $SSH_OPTS "$REMOTE_HOST" "docker rm -f abtop-k6 >/dev/null 2>&1" 2>/dev/null
  [ -n "$REMOTE_LOCK_PID" ] && kill "$REMOTE_LOCK_PID" 2>/dev/null
}
trap cleanup EXIT

take_local_lock "$TOPOLOGY"
take_remote_lock

if [ "$TOPOLOGY" = split ]; then
  ssh $SSH_OPTS "$REMOTE_HOST" "docker image inspect '$K6_IMAGE' >/dev/null 2>&1" \
    || die "$REMOTE_HOST 上没有 k6 镜像 $K6_IMAGE（先 docker save | ssh docker load）"
  local_sum=$(sha256sum "$K6_DIR/k6.js" 2>/dev/null | awk '{print $1}')
  remote_sum=$(ssh $SSH_OPTS "$REMOTE_HOST" "sha256sum '$K6_DIR/k6.js' 2>/dev/null | awk '{print \$1}'")
  [ -n "$local_sum" ] && [ "$local_sum" = "$remote_sum" ] \
    || die "k6.js 两端哈希不一致（本机 ${local_sum:-无} / $REMOTE_HOST ${remote_sum:-无}）"
  log "  split 前置通过：远端镜像与 k6.js 哈希一致"
fi

# ---------------------------------------------------------------- 前置自检
[ -x "$GATEWAY_DIR/xlite-$ARM" ] || die "找不到 $GATEWAY_DIR/xlite-$ARM"
[ -s "$CATALOG" ] || die "找不到目录文件 $CATALOG（要指向 127.0.0.1 的那份）"
grep -q '127\.0\.0\.1' "$CATALOG" || die "$CATALOG 不是指向 127.0.0.1 的版本"
log "ab_topology: arm=$ARM topo=$TOPOLOGY kb=$KEEP_BODY levels=[$LEVELS] twt=$TWT conns=$CONNS reload=$RELOAD"
K6_REF=$(docker image inspect -f '{{index .RepoDigests 0}}' "$K6_IMAGE" 2>/dev/null)
[ -n "$K6_REF" ] || K6_REF=$(docker image inspect -f '{{.Id}}' "$K6_IMAGE" 2>/dev/null)
log "  k6 镜像=$K6_IMAGE ref=${K6_REF:-n/a}"
log "  k6.js sha256=$(sha256sum "$K6_DIR/k6.js" 2>/dev/null | cut -c1-16)…"

# ---------------------------------------------------------------- 网关
GW_PORT_DB="$GATEWAY_DIR/$ARM-topo.db"
rm -f "$GW_PORT_DB"
( cd /tmp && DATABASE_URL="sqlite:$GW_PORT_DB" "$GATEWAY_DIR/xlite-$ARM" import "$CATALOG" --replace > "$OUT/import.log" 2>&1 ) \
  || die "import 失败：$(tail -1 "$OUT/import.log")"
log "  catalog: $(tail -1 "$OUT/import.log")"

pid=$(ss -lntp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
[ -n "$pid" ] && { kill "$pid" 2>/dev/null; sleep 2; }
env DATABASE_URL="sqlite:$GW_PORT_DB" PORT="$PORT" LISTEN_HOST=0.0.0.0 \
    XLITE_UPSTREAM_H2C=1 XLITE_CATALOG_RELOAD_SECS="$RELOAD" \
    TOKIO_WORKER_THREADS="$TWT" XLITE_UPSTREAM_MAX_CONNS_PER_HOST="$CONNS" \
  setsid nohup "$GATEWAY_DIR/xlite-$ARM" > "$OUT/gw.log" 2>&1 < /dev/null &
for _ in $(seq 1 30); do sleep 1; curl -s -m3 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break; done
gwpid=$(ss -lntp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
case "$gwpid" in ''|*[!0-9]*) die "网关没起来";; esac
log "  gw: pid=$gwpid backlog=$(ss -lnt "sport = :$PORT" 2>/dev/null | awk 'NR>1{print $3; exit}') health=$(curl -s -m3 "http://127.0.0.1:$PORT/health")"

# ---------------------------------------------------------------- mock 农场 PID
mapfile -t MPIDS < <(for p in $(expand_ports "$MOCK_PORTS"); do
  docker inspect -f '{{.State.Pid}}' "${MOCK_CONTAINER_PREFIX}$p" 2>/dev/null; done)
[ "${#MPIDS[@]}" -gt 0 ] || die "拿不到任何 mock 容器 PID（容器名前缀 $MOCK_CONTAINER_PREFIX）"
log "  mock: ${#MPIDS[@]} 个容器"

# ---------------------------------------------------------------- CPU 采样器
# 每秒记一次「累计 CPU tick（100 Hz）」：本机网关 + 本机农场（求和）[+ 本机 k6]
cat > /tmp/ab_topology_sampler.sh <<'SAMPLER'
#!/bin/bash
out="$1"; gw="$2"; k6f="$3"; shift 3
: > "$out"
while [ -r "/proc/$gw/stat" ]; do
  t=$(date +%s)
  g=$(awk '{print $14+$15}' "/proc/$gw/stat" 2>/dev/null || echo 0)
  k=0; kp=$(cat "$k6f" 2>/dev/null || echo 0)
  [ -n "$kp" ] && [ "$kp" != 0 ] && k=$(awk '{print $14+$15}' "/proc/$kp/stat" 2>/dev/null || echo 0)
  f=0
  for p in "$@"; do v=$(awk '{print $14+$15}' "/proc/$p/stat" 2>/dev/null) && f=$((f+v)); done
  echo "$t $g ${f:-0} ${k:-0}" >> "$out"
  sleep 1
done
SAMPLER
chmod +x /tmp/ab_topology_sampler.sh
echo 0 > /tmp/ab_topology_k6pid
setsid nohup bash /tmp/ab_topology_sampler.sh "$OUT/cpu.log" "$gwpid" /tmp/ab_topology_k6pid "${MPIDS[@]}" \
  >/dev/null 2>&1 < /dev/null &
sleep 2

cores_between() {  # $1=log $2=列号 $3=t0 $4=t1
  awk -v f="$2" -v a="$3" -v b="$4" '
    $1>=a && $1<=b { if (!n++) { t0=$1; v0=$f } t1=$1; v1=$f }
    END { if (n>1 && t1>t0) printf "%.2f", (v1-v0)/100/(t1-t0); else print "-" }' "$1"
}

# ---------------------------------------------------------------- 跑各档
if [ "$TOPOLOGY" = local ]; then TARGET_HOST=127.0.0.1; else TARGET_HOST=$(hostname -I | awk '{print $1}'); fi
for L in $LEVELS; do
  t0=$(date +%s)
  log "level $L 开始（$TOPOLOGY，目标 $TARGET_HOST:$PORT）"
  docker rm -f abtop-k6 >/dev/null 2>&1
  [ "$TOPOLOGY" = split ] && ssh $SSH_OPTS "$REMOTE_HOST" "docker rm -f abtop-k6 >/dev/null 2>&1" 2>/dev/null
  K6ARGS=(-d --name abtop-k6 --network=host -m "$K6_MEM" -v "$K6_DIR:/k6"
    -e "TARGET_URL=http://$TARGET_HOST:$PORT/v1/chat/completions" -e "TOKEN=$TOKEN" -e "MODEL=$MODEL"
    -e STREAM=true -e IN_TOKENS=100 -e OUT_TOKENS=500 -e "VUS=$L" -e "DURATION=$DURATION" -e "MAX_VUS=$L"
    -e STREAM_TIMEOUT=600000 -e "WARMUP=$WARMUP" -e "KEEP_BODY=$KEEP_BODY" -e "GRACEFUL_STOP=$GRACEFUL_STOP"
    "$K6_IMAGE" run --compatibility-mode=base
    --summary-trend-stats='p(50),p(90),p(95),p(99),avg,min,max' /k6/k6.js)
  if [ "$TOPOLOGY" = local ]; then
    docker run "${K6ARGS[@]}" >/dev/null 2>&1
    K6HOST=$(hostname); K6NAME=abtop-k6
  else
    printf '%q ' "${K6ARGS[@]}" | ssh $SSH_OPTS "$REMOTE_HOST" 'bash -s' >/dev/null 2>&1
    K6HOST=$REMOTE_HOST; K6NAME=abtop-k6
  fi
  if [ "$K6HOST" = "$(hostname)" ]; then
    k6run() { docker "$@"; }          # 本机
  else
    k6run() { ssh $SSH_OPTS "$REMOTE_HOST" "docker $(printf '%q ' "$@")"; }   # 远端
  fi
  # ★ 护栏：容器必须真的起来了（否则整轮会静默失败，产出一行假数据）
  ok=0
  for _ in $(seq 1 25); do
    [ "$(k6run inspect -f '{{.State.Running}}' "$K6NAME" 2>/dev/null)" = "true" ] && { ok=1; break; }
    sleep 1
  done
  if [ "$ok" != 1 ]; then
    log "  !! level $L 的 k6 容器没起来，记录后跳过"
    k6run logs "$K6NAME" 2>&1 | tail -6 | sed 's/^/    /' | tee -a "$OUT/progress.log"
    k6run rm -f "$K6NAME" >/dev/null 2>&1
    continue
  fi
  s=$(k6run inspect -f '{{.State.Pid}}' "$K6NAME" 2>/dev/null || echo 0)
  [ "$TOPOLOGY" = local ] && echo "$s" > /tmp/ab_topology_k6pid
  sleep 20
  # k6 真值：docker stats 比 /proc 采样稳（/proc 采过假 0）
  k6cpu=$(k6run stats --no-stream --format '{{.CPUPerc}}' "$K6NAME" 2>/dev/null)
  k6run wait "$K6NAME" >/dev/null 2>&1
  t1=$(date +%s)
  k6run logs "$K6NAME" > "$OUT/$L.log" 2>&1
  k6run rm -f "$K6NAME" >/dev/null 2>&1
  echo 0 > /tmp/ab_topology_k6pid
  log "  $L 结束 dur=$((t1-t0))s  cores: gw=$(cores_between "$OUT/cpu.log" 2 "$t0" "$t1") farm=$(cores_between "$OUT/cpu.log" 3 "$t0" "$t1") k6(本机采样)=$(cores_between "$OUT/cpu.log" 4 "$t0" "$t1") k6(docker stats@运行中)=$k6cpu"
  grep -E "gateway_overhead_ms|http_req_duration\.|iteration_duration\.|http_reqs\.|http_req_failed" "$OUT/$L.log" \
    | sed "s/^/  $L /" | tee -a "$OUT/progress.log"
done
log "ab_topology 完成"
