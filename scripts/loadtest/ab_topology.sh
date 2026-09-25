#!/bin/bash
# =============================================
# ab_topology.sh —— 把链路从路径上拿掉，找**瓶颈落在哪一组进程**。
#
# 三个 harness 的分工：
#   run_levels.sh      对标 New API 的定档位压测（要 gateway_overhead_ms，带报告闸门）
#   ab_configs.sh      判某个**旋钮/改动**值不值（判决指标含网关 CPU）
#   ab_topology.sh     判**瓶颈在谁身上**：网关 / mock 农场 / k6 各吃多少核
#
# ## 为什么需要它
#
# 链路受限的台架上先撞顶的永远是网卡 —— 实测 1 GbE 拐点在 **≈990 req/s ≈100 MB/s**
# （1 GbE 实用带宽的 ~90%，见 docs/manual/v88-rig.md §2），什么都测不出来。
# 把 k6 / 网关 / 24 mock **全放到同一台机**（loopback）就能把链路从路径上拿掉，
# 然后**分别**采三组进程的 CPU，看谁先饱和。
#
# ## 它测出来的结论（docs/dev/loadtest-newapi-controlled-2026-09.md §13）
#
#   **瓶颈是 mock 农场，不是网关。** 归一成「每千请求/秒消耗的核数」：
#     农场 34–38、k6 12–13、网关 6–7（农场是网关的约 5 倍）；
#     16000 VU 同机时农场独吃 72 核、网关 9.45 核，且 **fail 始终 0.00%**
#     （网关只是在排队，从不拒连）。⇒ **要扩的是农场。**
#
# ## 用法
#
#   # 仓库内跑（默认目录文件取 examples/mock-24.yaml，它就是 127.0.0.1 版）
#   ARM=A2 LEVELS="1000 4000 8000" scripts/loadtest/ab_topology.sh
#
#   # 仓库外跑（例如把脚本拷到 lab 的 /tmp）：**必须显式传 CATALOG**
#   CATALOG=$HOME/ab9/mock-24.yaml ARM=A2 LEVELS="4000" /tmp/ab_topology.sh
#
# ## 环境变量
#
#   CATALOG=<path>            指向 127.0.0.1 的 24-mock 目录 YAML。仓库外跑必须传。
#   GATEWAY_DIR=$HOME/ab6     网关机上的目录：要有 xlite-<ARM>
#   ARM=A2                    用哪个二进制（A0/A1/D/A2/L/S…）
#   PORT=18120
#   LEVELS="1000 4000 8000 16000 31000"
#   DURATION=120s  WARMUP=20  GRACEFUL_STOP=20s
#   KEEP_BODY=false           ★压瓶颈模式用 false。true 是对标模式：k6 要逐字节解析
#                             502 个 chunk，会**先于网关饱和** —— 实测 qps 在 4000 VU
#                             就封顶 ~313，白丢约 4 倍吞吐（见 §13.1）。两件事要分开跑。
#   TWT=8 CONNS=32 RELOAD=0   → TOKIO_WORKER_THREADS / XLITE_UPSTREAM_MAX_CONNS_PER_HOST
#                             / XLITE_CATALOG_RELOAD_SECS（0=冻结快照，去掉 reload 噪声）
#   MOCK_PORTS="4101-4124"    MOCK_CONTAINER_PREFIX=mock-h2c-
#   K6_DIR=$HOME/labtest/k6dir2  K6_IMAGE=k6-customer:latest  K6_MEM=45g
#   TOKEN=sk-loadtest  MODEL=mock
#   OUT=$HOME/labtest/ab-topology
#
# ## 产物（即一轮评测的工件）
#
#   progress.log   每档：三组进程的核数 + qps/dur/fail
#   <LEVEL>.log    k6 原始输出
#   cpu.log        每秒一行 `<epoch> <网关tick> <农场tick和>`（/proc/<pid>/stat）；k6 核数走 `docker stats`
#
# ## 判读纪律（v88-rig.md §9.1）
#
#   CPU 采样抖动约 1% ⇒ 差异要 > ~2% 才算数；延迟的窗口漂移可达 −3% ~ −9.8%
#   ⇒ 延迟差异必须**同窗口交错 ≥3 轮**才能认。本脚本给的是 CPU 归因，不是延迟判决。
# =============================================

set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib_rig.sh
. "$SCRIPT_DIR/lib_rig.sh"

REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
CATALOG=${CATALOG:-}
GATEWAY_DIR=${GATEWAY_DIR:-$HOME/ab6}
ARM=${ARM:-A2}
PORT=${PORT:-18120}
LEVELS=${LEVELS:-"1000 4000 8000 16000 31000"}
DURATION=${DURATION:-120s}
WARMUP=${WARMUP:-20}
GRACEFUL_STOP=${GRACEFUL_STOP:-20s}
KEEP_BODY=${KEEP_BODY:-false}
# 只在 KEEP_BODY=true（对标模式）时有意义：决定脚本解析 body 的比例（checks 比 2.00 / 1.10）。
# **显式传**，不靠 k6.js 的文件默认 —— 仓库版默认 1，客户机上的 memopt 副本默认 0.1，
# 「不传」等于让「跑的是哪份文件」决定喂法（见 docs/dev/loadtest-newapi-controlled-2026-09.md §13.6 第 1b 条）。
KEEP_SAMPLE=${KEEP_SAMPLE:-1}
TWT=${TWT:-8}
CONNS=${CONNS:-32}
RELOAD=${RELOAD:-0}
MOCK_PORTS=${MOCK_PORTS:-"4101-4124"}
MOCK_CONTAINER_PREFIX=${MOCK_CONTAINER_PREFIX:-mock-h2c-}
K6_DIR=${K6_DIR:-$HOME/labtest/k6dir2}
K6_IMAGE=${K6_IMAGE:-k6-customer:latest}
K6_MEM=${K6_MEM:-45g}
TOKEN=${TOKEN:-sk-loadtest}
MODEL=${MODEL:-mock}
OUT=${OUT:-$HOME/labtest/ab-topology}

mkdir -p "$OUT"
log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/progress.log"; }

# ★ 网关的 fd 上限**不是**无关紧要的：网关由本脚本直接拉起，会继承 shell 的 `nofile`，
#   而 ssh 非交互 shell 默认只有 **1024**。被静默卡住的表现是「迭代时长、scenario、
#   fail% 全都正常，但吞吐只有 1/4」—— 实测 4000 VU 时只跑出 865 流、qps 147（应为 621）。
#   注意：这条**不是**「k6 在容器里所以无关」—— 对 k6 无关，对网关要命。
ulimit -n 1048576 2>/dev/null || true
log "  shell nofile=$(ulimit -n)"
die() { printf '✗ %s\n' "$*" >&2; exit 1; }

lab_lock "ab_topology" || exit 1

# ★ 清理必须在**启动任何子进程之前**注册好。返工第一版把 trap 放在启网关之后，
#   于是 `die`（例如 nofile 太小）会直接 exit、cleanup 从不执行 ⇒ 留下
#   ① 占着 $PORT 的孤儿网关，② 该网关继承了 fd 9 ⇒ **台架锁被永久持有**。
#   子进程一律用 `9>&-` 显式关掉锁 fd（不然网关活着就等于替我们攥着锁）。
SAMPLER_PID=""
GW_STARTED=0
stop_gw() {
  # 只在**本次真的起过**网关时才杀。否则前置自检 die（例如 CATALOG 写错）会顺手
  # 杀掉 lab 上别人正监听 $PORT 的网关。
  [ "$GW_STARTED" = 1 ] || return 0
  local p
  p=$(ss -lntp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
  [ -n "$p" ] && kill "$p" 2>/dev/null
}
cleanup() {
  [ -n "${SAMPLER_PID:-}" ] && kill "$SAMPLER_PID" 2>/dev/null
  docker rm -f abtop-k6 >/dev/null 2>&1
  stop_gw
}
trap cleanup EXIT

# ---------------------------------------------------------------- 前置自检
expand_ports() {
  case "$1" in
    [0-9]*-[0-9]*) seq "${1%-*}" "${1#*-}" ;;
    *) printf '%s' "$1" | tr ',' ' ' ;;
  esac
}

# 目录文件必须存在且指向 127.0.0.1（同机 loopback）。仓库内跑默认取 examples/ 那份；
# 仓库外跑（脚本被拷到别处）必须显式传 CATALOG —— 报错里列出**试过哪些路径**，
# 免得下次又要在 lab 上猜（真实踩过：以为默认能找到，实际死在开跑前）。
if [ -z "$CATALOG" ]; then
  for c in "$REPO_ROOT/examples/mock-24.yaml" "$PWD/mock-24.yaml"; do
    [ -s "$c" ] && { CATALOG=$c; break; }
  done
fi
{ [ -n "$CATALOG" ] && [ -s "$CATALOG" ]; } || die "找不到目录文件。试过：
     CATALOG='${CATALOG:-未设置}'、$REPO_ROOT/examples/mock-24.yaml、$PWD/mock-24.yaml
   → 仓库外跑请显式传 CATALOG=<指向 127.0.0.1 的 mock-24.yaml>"
grep -q '127\.0\.0\.1' "$CATALOG" || die "$CATALOG 不是指向 127.0.0.1 的版本（同机 loopback 需要它）"
[ -x "$GATEWAY_DIR/xlite-$ARM" ] || die "找不到可执行 $GATEWAY_DIR/xlite-$ARM"
[ -s "$K6_DIR/k6.js" ] || die "找不到 $K6_DIR/k6.js"

K6_REF=$(docker image inspect -f '{{index .RepoDigests 0}}' "$K6_IMAGE" 2>/dev/null)
[ -n "$K6_REF" ] || K6_REF=$(docker image inspect -f '{{.Id}}' "$K6_IMAGE" 2>/dev/null)
log "ab_topology: arm=$ARM kb=$KEEP_BODY sample=$KEEP_SAMPLE levels=[$LEVELS] twt=$TWT conns=$CONNS reload=$RELOAD"
log "  catalog=$CATALOG  k6=$K6_IMAGE ref=${K6_REF:-n/a}  k6.js sha256=$(sha256sum "$K6_DIR/k6.js" | cut -c1-16)…"

# ---------------------------------------------------------------- 网关（本机）
GW_DB="$GATEWAY_DIR/$ARM-topo.db"
rm -f "$GW_DB"
( cd /tmp && DATABASE_URL="sqlite:$GW_DB" "$GATEWAY_DIR/xlite-$ARM" import "$CATALOG" --replace > "$OUT/import.log" 2>&1 ) \
  || die "import 失败：$(tail -1 "$OUT/import.log")"
log "  catalog: $(tail -1 "$OUT/import.log")"

stop_gw; sleep 2   # 清掉端口上可能残留的旧网关（trap 已注册，die 也会收拾）
env DATABASE_URL="sqlite:$GW_DB" PORT="$PORT" LISTEN_HOST=127.0.0.1 \
    XLITE_UPSTREAM_H2C=1 XLITE_CATALOG_RELOAD_SECS="$RELOAD" \
    TOKIO_WORKER_THREADS="$TWT" XLITE_UPSTREAM_MAX_CONNS_PER_HOST="$CONNS" \
  setsid nohup "$GATEWAY_DIR/xlite-$ARM" > "$OUT/gw.log" 2>&1 < /dev/null 9>&- &
GW_STARTED=1
for _ in $(seq 1 30); do sleep 1; curl -s -m3 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break; done
gwpid=$(ss -lntp 2>/dev/null | grep ":$PORT " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
case "$gwpid" in ''|*[!0-9]*) die "网关没起来（看 $OUT/gw.log）";; esac
gw_nofile=$(awk '/Max open files/{print $4}' "/proc/$gwpid/limits" 2>/dev/null)
log "  gw: pid=$gwpid backlog=$(ss -lnt "sport = :$PORT" 2>/dev/null | awk 'NR>1{print $3; exit}') nofile=$gw_nofile health=$(curl -s -m3 "http://127.0.0.1:$PORT/health")"
# 让静默封顶变成硬失败：nofile 低于 65536 时，高 VU 档的连接会被悄悄挡住
case "$gw_nofile" in
  ''|*[!0-9]*) log "  ⚠️ 读不到网关 nofile，无法确认没有被卡住";;
  *) [ "$gw_nofile" -lt 65536 ] && die "网关 nofile=$gw_nofile 太小 —— 连接数会被**静默**卡住
     （实测 nofile=1024 时 4000 VU 只跑出 865 流、qps 掉到 1/4，而 scenario/fail% 全正常）。
     先 `ulimit -n 1048576` 再起网关。";;
esac

# ---------------------------------------------------------------- mock 农场 PID
mapfile -t MPIDS < <(for p in $(expand_ports "$MOCK_PORTS"); do
  docker inspect -f '{{.State.Pid}}' "${MOCK_CONTAINER_PREFIX}$p" 2>/dev/null; done)
[ "${#MPIDS[@]}" -gt 0 ] || die "拿不到任何 mock 容器 PID（前缀 $MOCK_CONTAINER_PREFIX）"
log "  mock: ${#MPIDS[@]} 个容器"

# ---------------------------------------------------------------- CPU 采样器
# 每秒一行累计 tick（100 Hz）。记下自己的 PID，收尾只 kill 这个 PID
# （此前用 `pkill -f "ab_topology.*sampler"`，匹配面太宽，会误杀别人 session 的同名采样器）
# **只采本机的网关与农场**（两列）。k6 不在这里采：它在容器里，`/proc` 采样实测会采到
# `0.474` / `0.000` 这种假值（采集竞态），所以 k6 的核数一律用运行中 `docker stats`。
SAMPLER=/tmp/ab_topology_sampler.sh
cat > "$SAMPLER" <<'SAMPLER_BODY'
#!/bin/bash
# $1=out $2=gw_pid ; 其余 = mock 宿主 PID
out="$1"; gw="$2"; shift 2
: > "$out"
while [ -r "/proc/$gw/stat" ]; do
  t=$(date +%s)
  g=$(awk '{print $14+$15}' "/proc/$gw/stat" 2>/dev/null || echo 0)
  f=0
  for p in "$@"; do v=$(awk '{print $14+$15}' "/proc/$p/stat" 2>/dev/null) && f=$((f+v)); done
  echo "$t $g ${f:-0}" >> "$out"
  sleep 1
done
SAMPLER_BODY
chmod +x "$SAMPLER"
setsid nohup bash "$SAMPLER" "$OUT/cpu.log" "$gwpid" "${MPIDS[@]}" \
  >/dev/null 2>&1 < /dev/null 9>&- &
SAMPLER_PID=$!
sleep 2

cores_between() {  # $1=log $2=列号 $3=t0 $4=t1
  awk -v f="$2" -v a="$3" -v b="$4" '
    $1>=a && $1<=b { if (!n++) { t0=$1; v0=$f } t1=$1; v1=$f }
    END { if (n>1 && t1>t0) printf "%.2f", (v1-v0)/100/(t1-t0); else print "-" }' "$1"
}

# ---------------------------------------------------------------- 各档
FAILED_LEVELS=0
for L in $LEVELS; do
  t0=$(date +%s)
  log "level $L 开始（同机 loopback，目标 127.0.0.1:$PORT）"
  docker rm -f abtop-k6 >/dev/null 2>&1
  docker run -d --name abtop-k6 --network=host -m "$K6_MEM" -v "$K6_DIR:/k6" \
    -e "TARGET_URL=http://127.0.0.1:$PORT/v1/chat/completions" \
    -e "TOKEN=$TOKEN" -e "MODEL=$MODEL" -e STREAM=true \
    -e IN_TOKENS=100 -e OUT_TOKENS=500 -e "VUS=$L" -e "DURATION=$DURATION" -e "MAX_VUS=$L" \
    -e STREAM_TIMEOUT=600000 -e "WARMUP=$WARMUP" -e "KEEP_BODY=$KEEP_BODY" \
    -e "KEEP_SAMPLE=$KEEP_SAMPLE" \
    -e "GRACEFUL_STOP=$GRACEFUL_STOP" \
    "$K6_IMAGE" run --compatibility-mode=base \
    --summary-trend-stats='p(50),p(90),p(95),p(99),avg,min,max' /k6/k6.js >/dev/null 2>&1

  # ★ 护栏：容器必须真的起来了。此前把 docker run 的输出丢进 /dev/null，
  #   而 k6 机上既无镜像也无脚本 ⇒ 每档 42 秒、cores=0、一行指标都没有，**整轮静默失败**。
  ok=0
  for _ in $(seq 1 25); do
    [ "$(docker inspect -f '{{.State.Running}}' abtop-k6 2>/dev/null)" = "true" ] && { ok=1; break; }
    sleep 1
  done
  if [ "$ok" != 1 ]; then
    FAILED_LEVELS=$((FAILED_LEVELS + 1))
    log "  !! level $L 的 k6 容器没起来，记录后跳过"
    docker logs abtop-k6 2>&1 | tail -6 | sed 's/^/    /' | tee -a "$OUT/progress.log"
    docker rm -f abtop-k6 >/dev/null 2>&1
    continue
  fi
  sleep 20
  # k6 的 CPU 真值：运行中 `docker stats --no-stream`。**不要用 /proc 采样** ——
  # 在 k6 上采过 0.474 / 0.000 这种假值（采集竞态）。
  k6cpu=$(docker stats --no-stream --format '{{.CPUPerc}}' abtop-k6 2>/dev/null)
  docker wait abtop-k6 >/dev/null 2>&1
  t1=$(date +%s)
  docker logs abtop-k6 > "$OUT/$L.log" 2>&1
  docker rm -f abtop-k6 >/dev/null 2>&1
  if ! grep -qE "http_reqs\.|iteration_duration\." "$OUT/$L.log" 2>/dev/null; then
    FAILED_LEVELS=$((FAILED_LEVELS + 1))
    log "  !! level $L 没有产出指标（k6 异常退出？），跳过"
    continue
  fi
  log "  $L 结束 dur=$((t1-t0))s  cores: gw=$(cores_between "$OUT/cpu.log" 2 "$t0" "$t1") farm=$(cores_between "$OUT/cpu.log" 3 "$t0" "$t1") k6(docker stats@运行中)=$k6cpu"
  grep -E "gateway_overhead_ms|http_req_duration\.|iteration_duration\.|http_reqs\.|http_req_failed" "$OUT/$L.log" \
    | sed "s/^/  $L /" | tee -a "$OUT/progress.log"
done

if [ "$FAILED_LEVELS" -gt 0 ]; then
  log "ab_topology 结束：$FAILED_LEVELS 档失败 ⇒ 退出码 2（别让下游把缺档当跑完）"
  exit 2
fi
log "ab_topology 完成"
