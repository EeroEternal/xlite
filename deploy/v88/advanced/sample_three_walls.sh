#!/usr/bin/env bash
# ============================================================
# sample_three_walls.sh —— 压测**进行中**，把"墙"一面一面地采出来（**只读**）
#
# ## 为什么需要它
#
# 0927 那轮 B 组的高载区，XG 与 NA 的表现形态**相反**，而只看 k6 结果分不出墙在哪：
#   XG：ttft p50 938→1751ms、dur p50 8.6→19.0s、fail 1.2→6.5%、ttft p99 只到 6.1s
#   NA：ttft p50 平 ~1.0s、dur p50 平 6.4–9.1s、fail ~0%、**ttft p99 10–28s**
# 形态上 NA 是"队排在首字之前"、XG 是"队排在流中段与连接层"，但**队排在哪台机的哪一层**
# 需要三面墙**同时**采数才能分辨：
#   A 农场每口事件循环：24 口 × ~620 流/口 ≈ 6.2 万次 write+flush/s/口（超出舒适区）
#   B k6 客户端 CPU  ：`iteration_duration − http_req_duration` 实测 6.1s/圈 @40000VU
#   C 网关 worker 调度：TWT=8 × 2 节点要驱动 1.5 万条流、每 chunk 一次 write
#
# **已被读代码/算数排除的**（别重复查）：
#   · 上游 h2 池 —— xlite 数据面上**唯一**的等待点是 `xlite-h2-pool` 的 `acquire`
#     (`crates/xlite-h2-pool/src/lib.rs:26`「at the cap it waits」)，阈值 = 32 conn ×
#     min(对端 SETTINGS=250, 预算) = **8000 流/口**，现场只有 ~620/口，差 13 倍。
#   · 链路带宽 —— 14890 流 × ~17KB/s ≈ **2.0 Gbps**。
#   · 网关 CPU 总量 —— 按归一值 ≈ 5.5 核（两节点各 ~2.7 核）。
#
# ## 怎么用：**在每台机上各跑一份**，用行首时间戳对齐（不 ssh、不改任何东西）
#
#   .29（k6 + 农场）：见 A 与 B，并对远端网关做两个只读探测
#   .24 / .26（网关）：见 C
#   .22（HAProxy）    ：见 accept 队列与内核丢弃计数器（"为什么是 RST"）
#
# 本仓**故意不 ssh 采远端**（见 `docs/changelog.md`：§13 已按评审删掉过一条
# 「远端 coproc 锁 + 逐参转发 + 跨机哈希校验」的支路，四段高风险代码换一条已知结论）。
# 所以远端只做两个**只读 HTTP 探测**，而这两个恰好就是证据本身：
#   · `GET /health` 的 `time_total` —— HAProxy 那条健康检查在量同一个东西；它一旦接近或
#     超过 `timeout check`（默认取 `inter`，现有 cfg 是 5s），就能解释 60000 档那
#     **8544 个 503**（≈782 qps × 11s 的窗口）。
#   · `time_connect` —— 新连接建立时间；accept 队列积压会直接体现在这里。
#
# 用法：
#   ./sample_three_walls.sh --seconds 300                       # 压测进行中；每 5s 一行
#   ./sample_three_walls.sh --seconds 0                         # 跑到 Ctrl-C（推荐：比压测早开、晚停）
#   ./sample_three_walls.sh --seconds 300 --gateways 10.56.88.24:8080,10.56.88.26:8080
#   ./sample_three_walls.sh --seconds 300 --entries 10.56.88.22:8005
#   ./sample_three_walls.sh --seconds 60 --interval 2 --out /root/walls.log
#
# **只读保证**：本脚本只做 `/proc` 读取、`docker ps/stats/inspect`、`ss -lnt`、`nstat -az`、
# `curl -o /dev/null GET`。不写配置、不起容器、不碰农场 `_config`、**不取台架锁**
#（它要能在别人跑压测的同时采样）。
#
# 环境变量：`PROC_ROOT`（默认 `/proc`，**仅本机自测用**）、`MOCKS`（只用于交叉核对口数）。
# 阈值全部写在下面的 TH_* 里，现场按实际 `TOKIO_WORKER_THREADS` 改 `TH_GW_TWT`。
#
# 退出码：0 = 采样完成（含 Ctrl-C）；2 = 参数错
# ============================================================
set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
V88_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
# shellcheck source=../lib/lib_v88.sh
. "$V88_DIR/lib/lib_v88.sh"

PROC_ROOT=${PROC_ROOT:-/proc}
HZ=$(getconf CLK_TCK 2>/dev/null || echo 100)
case "$HZ" in ''|*[!0-9]*) HZ=100 ;; esac

RUN_SECONDS=300
INTERVAL=5
OUT=""
GATEWAYS="10.56.88.24:8080,10.56.88.26:8080"
ENTRIES="10.56.88.22:8005"
DOCKER_STATS=1

TH_FARM_PCT=90        # 农场单口占一个核的百分比
TH_K6_NPROC_PCT=80    # k6 占 nproc 的百分比
TH_GW_TWT=8           # 网关实际的 TOKIO_WORKER_THREADS（CONFIG.md §1）
TH_GW_TWT_PCT=90      # 网关占 TWT 的百分比
TH_HEALTH_MS=5000     # HAProxy 的 timeout check（默认取 inter = 5s）

usage() { sed -n '2,52p' "$0"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --seconds) RUN_SECONDS="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --gateways) GATEWAYS="${2:-}"; shift 2 ;;
    --entries) ENTRIES="${2:-}"; shift 2 ;;
    --no-docker-stats) DOCKER_STATS=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) v88_die "不认识的参数：$1（--help 看用法）" ;;
  esac
done

case "$RUN_SECONDS" in ''|*[!0-9]*) v88_die "--seconds 要是整数（0 = 跑到 Ctrl-C），得到：${RUN_SECONDS}" ;; esac
case "$INTERVAL" in ''|*[!0-9]*) v88_die "--interval 要是整数秒，得到：${INTERVAL}" ;; esac
[ "$INTERVAL" -ge 1 ] || v88_die "--interval 至少 1 秒"
[ -d "$PROC_ROOT" ] || v88_die "读不到 ${PROC_ROOT}（本脚本要在 Linux 上跑；PROC_ROOT 只给本机自测用）"

NPROC=$(nproc 2>/dev/null || echo 0)
case "$NPROC" in ''|*[!0-9]*) NPROC=0 ;; esac

TMP=$(mktemp -d 2>/dev/null || echo "/tmp/three_walls.$$")
mkdir -p "$TMP" 2>/dev/null
[ -n "$OUT" ] && : > "$OUT"

out_line() { # 同时打 stdout 与 --out（人看着方便，文件用于事后对齐三台机）
  [ -n "$OUT" ] && printf '%s\n' "$*" >> "$OUT"
  printf '%s\n' "$*"
}

# ---------------------------------------------------------------- /proc 只读工具
# /proc/<pid>/stat 的 utime+stime（tick）。comm 可能含空格/括号，所以先切到 ") " 之后：
# 剩余字段从 field 3(state) 起 ⇒ utime 是第 12 个、stime 第 13 个。
ticks_of() {
  local line cut
  [ -r "$PROC_ROOT/$1/stat" ] || { printf '0'; return; }
  read -r line < "$PROC_ROOT/$1/stat" 2>/dev/null || { printf '0'; return; }
  cut=${line#*) }
  # 真 /proc 一定有 "(comm) "；万一没有（或 comm 里带 ')'）就别猜——吐 0，
  # 由 `multi_ticks` 的存活数 + tick 行的“!口数变了”标记把它暴露出来，不静默算错。
  [ "$cut" = "$line" ] && { printf '0'; return; }
  set -- $cut
  printf '%s' "$(( ${12:-0} + ${13:-0} ))"
}

# 传入 "pid:tag pid:tag …" ⇒ "合计tick 最大单进程tick 存活个数"
multi_ticks() {
  local total=0 max=0 n=0 t pid
  for e in $1; do
    pid=${e%%:*}
    t=$(ticks_of "$pid")
    [ "$t" -gt 0 ] || continue
    total=$((total + t)); n=$((n + 1))
    [ "$t" -gt "$max" ] && max=$t
  done
  printf '%s %s %s' "$total" "$max" "$n"
}

# /proc/stat 聚合行 ⇒ "idle steal total"
cpu_line() {
  local line
  # 先 `-r` 探一下：`read ... < 不存在的文件` 的重定向失败会**绕过 `2>/dev/null`**
  # 直接往 stderr 抄一行（冒烟测试实测：每 tick 一条噪音）
  [ -r "$PROC_ROOT/stat" ] || { printf '0 0 0'; return; }
  read -r line < "$PROC_ROOT/stat" 2>/dev/null || { printf '0 0 0'; return; }
  set -- $line
  printf '%s %s %s' "${5:-0}" "${9:-0}" \
    "$(( ${2:-0} + ${3:-0} + ${4:-0} + ${5:-0} + ${6:-0} + ${7:-0} + ${8:-0} + ${9:-0} ))"
}

rss_kb_of() { # 一律吐整数：进程/容器刚消失时 status 读不到，空串会让 $(( )) 报
  local v         # “arithmetic syntax error” 并**静默停止采样**（冒烟测试实测踩到过）
  v=$(awk '/^VmRSS:/{print $2; exit}' "$PROC_ROOT/$1/status" 2>/dev/null)
  case "$v" in ''|*[!0-9]*) v=0 ;; esac
  printf '%s' "$v"
}

cnt() { printf '%s' "$1" | wc -w | tr -d ' '; }

# ---------------------------------------------------------------- 角色识别（本机有什么采什么）
MOCK_PIDS=$(docker inspect -f '{{.Name}} {{.State.Pid}}' \
    $(docker ps -q --filter name=mock 2>/dev/null) 2>/dev/null \
  | sed 's|^/||' | awk '{n=$1; p=$2; sub(/^.*-/,"",n); if (p+0>0 && n ~ /^[0-9]+$/) printf "%s:%s ", p, n}')
MOCK_SRC="docker inspect（pid ↔ 端口）"
if [ -z "$MOCK_PIDS" ]; then
  MOCK_PIDS=$(pgrep -x mock-llm 2>/dev/null | awk '{printf "%s:?", $1}')
  [ -n "$MOCK_PIDS" ] && MOCK_SRC="pgrep -x mock-llm（端口未知，只能给合计）"
fi
MOCK_N0=$(cnt "$MOCK_PIDS")

K6_PIDS=""; K6_NAME=""
for cand in v88_k6 v88_scan_k6 abtop-k6; do
  ids=$(docker ps -q --filter "name=$cand" 2>/dev/null)
  [ -n "$ids" ] || continue
  # shellcheck disable=SC2086
  p=$(docker inspect -f '{{.State.Pid}}' $ids 2>/dev/null | grep -v '^0$' | awk '{printf "%s:k6 ", $1}')
  [ -n "$p" ] || continue
  K6_PIDS="$p"
  K6_NAME=$(docker ps --filter "name=$cand" --format '{{.Names}}' 2>/dev/null | head -1)
  break
done
if [ -z "$K6_PIDS" ]; then
  p=$(pgrep -f 'k6.js|k6 run' 2>/dev/null | awk '{printf "%s:k6 ", $1}')
  [ -n "$p" ] && { K6_PIDS="$p"; K6_NAME="(bare k6)"; }
fi

GW_PIDS=""; GW_NAME=""
ids=$(docker ps -q --filter 'name=xlite' 2>/dev/null)
if [ -n "$ids" ]; then
  # shellcheck disable=SC2086
  GW_PIDS=$(docker inspect -f '{{.State.Pid}}' $ids 2>/dev/null | grep -v '^0$' | awk '{printf "%s:gw ", $1}')
  GW_NAME=$(docker ps --filter 'name=xlite' --format '{{.Names}}' 2>/dev/null | head -1)
fi
if [ -z "$GW_PIDS" ]; then
  p=$(pgrep -x xlite 2>/dev/null | awk '{printf "%s:gw ", $1}')
  [ -n "$p" ] && { GW_PIDS="$p"; GW_NAME="(bare xlite)"; }
fi

HAP_PID=$(pgrep -x haproxy 2>/dev/null | head -1)
HOST_SHORT=$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo unknown)

SS_OK=0;  command -v ss >/dev/null 2>&1 && SS_OK=1
CURL_OK=0; command -v curl >/dev/null 2>&1 && CURL_OK=1
DSN_OK=0; [ "$DOCKER_STATS" = 1 ] && command -v docker >/dev/null 2>&1 && DSN_OK=1

NSTAT_TOOL=""
nstat_snap() {
  if command -v nstat >/dev/null 2>&1; then nstat -az > "$1" 2>/dev/null && { NSTAT_TOOL=nstat; return 0; }; fi
  if command -v netstat >/dev/null 2>&1; then netstat -s > "$1" 2>/dev/null && { NSTAT_TOOL=netstat; return 0; }; fi
  return 1
}
NSTAT_BEFORE=""
if [ -n "$HAP_PID" ] && nstat_snap "$TMP/nstat.before"; then NSTAT_BEFORE="$TMP/nstat.before"; fi

# ---------------------------------------------------------------- 头部
out_line "=== sample_three_walls @ ${HOST_SHORT} ==="
out_line "nproc=${NPROC}  HZ=${HZ}  每 ${INTERVAL}s 一次  时长=$([ "$RUN_SECONDS" = 0 ] && echo '直到 Ctrl-C' || echo "${RUN_SECONDS}s")"
if [ -n "$MOCK_PIDS" ]; then
  out_line "墙 A（农场）：本机可见 ${MOCK_N0} 口 —— ${MOCK_SRC}"
else
  out_line "墙 A（农场）：本机不可见 ⇒ 请到 mock 机上跑一份"
fi
if [ -n "$K6_PIDS" ]; then
  out_line "墙 B（k6）：本机可见 ${K6_NAME}"
else
  out_line "墙 B（k6）：本机不可见 ⇒ 请到 k6 机上跑一份"
fi
if [ -n "$GW_PIDS" ]; then
  out_line "墙 C（网关）：本机可见 ${GW_NAME}"
else
  out_line "墙 C（网关）：本机不可见 ⇒ 请到网关机上跑一份（本机只探远端 /health）"
fi
if [ -n "$HAP_PID" ]; then
  HAP_LIM=$(grep -i 'open files' "$PROC_ROOT/$HAP_PID/limits" 2>/dev/null | tr -s ' ')
  [ -n "$HAP_LIM" ] || HAP_LIM="(limits 读不到)"
  out_line "HAProxy：本机可见 pid=${HAP_PID}  ${HAP_LIM}"
  out_line "        ⇒ 同时采 accept 队列（ss -lnt 的 Recv-Q）与内核丢弃计数器（nstat 前后差）"
fi
[ -n "$GATEWAYS" ] && out_line "远端探测（健康检查口径）：${GATEWAYS}"
[ -n "$ENTRIES" ] && out_line "入口探测（connect 口径）：${ENTRIES}"
out_line "阈值：农场单口 > ${TH_FARM_PCT}% ｜ k6 > ${TH_K6_NPROC_PCT}%×nproc ｜ 网关 > ${TH_GW_TWT_PCT}%×TWT(${TH_GW_TWT}) ｜ /health > ${TH_HEALTH_MS}ms"
out_line "（单轮采样只定方向，不是判决 —— docs/manual/v88-rig.md §9.1）"
out_line ""

# ---------------------------------------------------------------- 单次采样
prev_cpu=$(cpu_line)
prev_farm="0 0 0"; prev_k6="0 0 0"; prev_gw="0 0 0"
prev_farm_n=0
prev_ts=$(date +%s)

# 先“预热”一次：把 prev_* 设成当前值，否则**第一行**会把 "utime+stime 的绝对值"
# 当成一个采样区间的增量（冒烟测试实测：farm=1.50c 与“0 口”同时出现——典型的假值）。
prime_prev() {
  prev_cpu=$(cpu_line)
  if [ -n "$MOCK_PIDS" ]; then
    prev_farm=$(multi_ticks "$MOCK_PIDS")
    prev_farm_n=$(printf '%s' "$prev_farm" | awk '{print $3}')
  fi
  [ -n "$K6_PIDS" ] && prev_k6=$(multi_ticks "$K6_PIDS")
  [ -n "$GW_PIDS" ] && prev_gw=$(multi_ticks "$GW_PIDS")
  prev_ts=$(date +%s)
}

one_tick() {
  # ⚠️ `now`/`ts` **不能**在这里取：本函数后半段有慢调用（`docker stats` ~1s、curl、ss），
  # 而 /proc 是逐段读的——`now` 必须跟在 /proc 读完之后、慢调用之前取，否则 dt 会被
  # 慢调用撑大而分子还是那些 tick，CPU 率**系统性偏低**（INTERVAL=5 + 1s 的 docker stats
  # 就偏差 ~20%）。
  local now dt ts line

  local farm="" k6="" gw="" cores

  # ---- 1) 先把 /proc 一次读完（快）----
  local farm_raw="" k6_raw="" gw_raw="" cpu_raw
  [ -n "$MOCK_PIDS" ] && farm_raw=$(multi_ticks "$MOCK_PIDS")
  [ -n "$K6_PIDS" ] && k6_raw=$(multi_ticks "$K6_PIDS")
  [ -n "$GW_PIDS" ] && gw_raw=$(multi_ticks "$GW_PIDS")
  cpu_raw=$(cpu_line)

  # ---- 2) 时间戳必须与上面那批读**同一时刻**（后面的慢调用不许挤进来）----
  now=$(date +%s); dt=$((now - prev_ts)); [ "$dt" -ge 1 ] || dt=1
  ts=$(date +%H:%M:%S)

  # ---- 3) 算增量并格式化 ----
  if [ -n "$MOCK_PIDS" ]; then
    local cur f_t f_m f_n d_t d_m pmax mark=""
    cur="$farm_raw"
    f_t=$(printf '%s' "$cur" | awk '{print $1}'); f_m=$(printf '%s' "$cur" | awk '{print $2}'); f_n=$(printf '%s' "$cur" | awk '{print $3}')
    d_t=$((f_t - $(printf '%s' "$prev_farm" | awk '{print $1}')))
    d_m=$((f_m - $(printf '%s' "$prev_farm" | awk '{print $2}')))
    cores=$(awk -v d="$d_t" -v t="$dt" -v h="$HZ" 'BEGIN{printf "%.2f", d/t/h}')
    pmax=$(awk -v d="$d_m" -v t="$dt" -v h="$HZ" 'BEGIN{printf "%.0f", d/t/h*100}')
    [ "$prev_farm_n" != 0 ] && [ "$f_n" != "$prev_farm_n" ] && mark="!口数变了"
    farm=$(printf 'farm=%sc(%s口/峰值%s%%%s)' "$cores" "$f_n" "$pmax" "$mark")
    printf '%s\n' "$cores" >> "$TMP/farm"
    printf '%s\n' "$pmax" >> "$TMP/farm_pmax"
    prev_farm="$f_t $f_m $f_n"; prev_farm_n=$f_n
  fi

  if [ -n "$K6_PIDS" ]; then
    local cur k_t k_m k_n d_t rss
    cur="$k6_raw"
    k_t=$(printf '%s' "$cur" | awk '{print $1}'); k_m=$(printf '%s' "$cur" | awk '{print $2}' ); k_n=$(printf '%s' "$cur" | awk '{print $3}')
    d_t=$((k_t - $(printf '%s' "$prev_k6" | awk '{print $1}')))
    cores=$(awk -v d="$d_t" -v t="$dt" -v h="$HZ" 'BEGIN{printf "%.2f", d/t/h}')
    rss=0; for e in $K6_PIDS; do rss=$((rss + $(rss_kb_of "${e%%:*}"))); done
    k6=$(printf 'k6=%sc/%sM' "$cores" "$((rss / 1024))")
    printf '%s\n' "$cores" >> "$TMP/k6"
    prev_k6="$k_t $k_m $k_n"
  fi

  if [ -n "$GW_PIDS" ]; then
    local cur g_t g_m g_n d_t rss
    cur="$gw_raw"
    g_t=$(printf '%s' "$cur" | awk '{print $1}'); g_m=$(printf '%s' "$cur" | awk '{print $2}'); g_n=$(printf '%s' "$cur" | awk '{print $3}')
    d_t=$((g_t - $(printf '%s' "$prev_gw" | awk '{print $1}')))
    cores=$(awk -v d="$d_t" -v t="$dt" -v h="$HZ" 'BEGIN{printf "%.2f", d/t/h}')
    rss=0; for e in $GW_PIDS; do rss=$((rss + $(rss_kb_of "${e%%:*}"))); done
    gw=$(printf 'gw=%sc(%s进程)/%sM' "$cores" "$g_n" "$((rss / 1024))")
    printf '%s\n' "$cores" >> "$TMP/gw"
    prev_gw="$g_t $g_m $g_n"
  fi

  local cur_c idle_d steal_d tot_d idle steal
  cur_c="$cpu_raw"
  idle_d=$(( $(printf '%s' "$cur_c" | awk '{print $1}') - $(printf '%s' "$prev_cpu" | awk '{print $1}') ))
  steal_d=$(( $(printf '%s' "$cur_c" | awk '{print $2}') - $(printf '%s' "$prev_cpu" | awk '{print $2}') ))
  tot_d=$(( $(printf '%s' "$cur_c" | awk '{print $3}') - $(printf '%s' "$prev_cpu" | awk '{print $3}') ))
  [ "$tot_d" -ge 1 ] || tot_d=1
  idle=$(awk -v a="$idle_d" -v b="$tot_d" 'BEGIN{printf "%.1f", a*100/b}')
  steal=$(awk -v a="$steal_d" -v b="$tot_d" 'BEGIN{printf "%.1f", a*100/b}')
  printf '%s\n' "$idle" >> "$TMP/idle"; printf '%s\n' "$steal" >> "$TMP/steal"
  prev_cpu="$cur_c"

  # ---- 4) 慢调用（docker stats ~1s / curl / ss）—— 已经不影响 dt 了 ----
  if [ "$DSN_OK" = 1 ] && [ -n "$k6" ]; then
    local ds
    ds=$(docker stats --no-stream --format '{{.CPUPerc}}' "$K6_NAME" 2>/dev/null | tr -d '%')
    [ -n "$ds" ] && k6="${k6}(ds=${ds}%)"
  fi

  local probes=""
  if [ "$CURL_OK" = 1 ]; then
    local ep tag v rc
    for ep in $(printf '%s' "$GATEWAYS" | tr ',' ' '); do
      [ -n "$ep" ] || continue
      tag=$(printf '%s' "$ep" | awk -F'[:.]' '{print $(NF-1)}')
      v=$(curl -sS -o /dev/null -m 5 -w '%{time_total} %{time_connect}' "http://$ep/health" 2>/dev/null); rc=$?
      if [ "$rc" = 0 ]; then
        probes="${probes} H${tag}=$(printf '%s' "$v" | awk '{printf "%.1fms/c%.1fms", $1*1000, $2*1000}')"
        printf '%s\n' "$(printf '%s' "$v" | awk '{printf "%.1f", $1*1000}')" >> "$TMP/health"
      else
        probes="${probes} H${tag}=FAIL(rc=${rc})"
        printf '99999\n' >> "$TMP/health"
      fi
    done
    for ep in $(printf '%s' "$ENTRIES" | tr ',' ' '); do
      [ -n "$ep" ] || continue
      tag=$(printf '%s' "$ep" | awk -F'[:.]' '{print $(NF-1)}')
      v=$(curl -sS -o /dev/null -m 5 -w '%{time_connect}' "http://$ep/health" 2>/dev/null); rc=$?
      if [ "$rc" = 0 ]; then
        probes="${probes} E${tag}=c$(printf '%s' "$v" | awk '{printf "%.1fms", $1*1000}')"
        printf '%s\n' "$(printf '%s' "$v" | awk '{printf "%.1f", $1*1000}')" >> "$TMP/entry_connect"
      else
        probes="${probes} E${tag}=FAIL(rc=${rc})"
      fi
    done
  fi

  local listen=""
  if [ "$SS_OK" = 1 ]; then
    local rq
    rq=$(ss -lnt 2>/dev/null | awk 'NR>1 && $2+0>0 {printf "%s:RQ=%s ", $4, $2}')
    [ -n "$rq" ] && listen="|积压[${rq}]"
  fi

  line="[${ts}]"
  [ -n "$farm" ] && line="${line} ${farm}"
  [ -n "$k6" ] && line="${line} ${k6}"
  [ -n "$gw" ] && line="${line} ${gw}"
  load=$(awk '{print $1}' "$PROC_ROOT/loadavg" 2>/dev/null); [ -n "$load" ] || load=-
  line="${line} cpu=idle${idle}/steal${steal} load=${load}"
  [ -n "$probes" ] && line="${line} |${probes}"
  [ -n "$listen" ] && line="${line} ${listen}"
  out_line "$line"

  prev_ts=$now
}

# ---------------------------------------------------------------- 主循环
STOPPED=0
trap 'STOPPED=1' INT TERM
prime_prev
sleep "$INTERVAL"          # 让**每一行**都覆盖一个完整采样区间（否则第一行的区间≈0）
deadline=$(( $(date +%s) + RUN_SECONDS ))
while :; do
  one_tick
  [ "$STOPPED" = 1 ] && break
  [ "$RUN_SECONDS" -gt 0 ] && [ "$(date +%s)" -ge "$deadline" ] && break
  sleep "$INTERVAL"
done

# ---------------------------------------------------------------- 汇总
stat3() { # <file> ⇒ "min/中位/max"（中位取上中位）
  [ -s "$1" ] || { printf -- '-'; return; }
  sort -n "$1" | awk '{a[NR]=$1} END{printf "%s/%s/%s", a[1], a[int((NR+1)/2)], a[NR]}'
}
peak() { [ -s "$1" ] && sort -n "$1" | tail -1 || printf ''; }

out_line ""
out_line "=== 汇总（min/中位/max，单位=核 或 % 或 ms）==="
[ -s "$TMP/farm" ] && out_line "农场合计核数     : $(stat3 "$TMP/farm")"
[ -s "$TMP/farm_pmax" ] && out_line "农场单口峰值(%核): $(stat3 "$TMP/farm_pmax")"
[ -s "$TMP/k6" ] && out_line "k6 核数          : $(stat3 "$TMP/k6")"
[ -s "$TMP/gw" ] && out_line "网关核数         : $(stat3 "$TMP/gw")"
[ -s "$TMP/idle" ] && out_line "整机 idle %      : $(stat3 "$TMP/idle")"
[ -s "$TMP/steal" ] && out_line "整机 steal %     : $(stat3 "$TMP/steal")"
[ -s "$TMP/health" ] && out_line "/health ms       : $(stat3 "$TMP/health")（99999 = 探测失败/超时）"
[ -s "$TMP/entry_connect" ] && out_line "入口 connect ms  : $(stat3 "$TMP/entry_connect")"

if [ -n "$NSTAT_BEFORE" ] && nstat_snap "$TMP/nstat.after"; then
  out_line ""
  if [ "$NSTAT_TOOL" = netstat ]; then
    # netstat -s 不是 `name value rate` 三列，不能套同一个 awk（会打出垃圾）⇒ 直接文本 diff
    out_line "--- 内核计数器变化（netstat -s 文本 diff，只看相关项）---"
    diff "$NSTAT_BEFORE" "$TMP/nstat.after" 2>/dev/null | grep '^[<>]' \
      | grep -iE 'listen|backlog|abort|reset|drop|overflow|timeout|retrans|syn' | head -25 \
      || out_line "  （无相关变化）"
  else
    out_line "--- 内核计数器增量（nstat -az 前后差，非零才列出）---"
    awk '
      FNR==NR { before[$1]=$2; next }
      { v=$2+0; b=before[$1]+0; if (v-b>0) printf "  %-34s +%d\n", $1, v-b }
    ' "$NSTAT_BEFORE" "$TMP/nstat.after" \
      | grep -iE 'listen|backlog|abort|reset|drop|overflow|timeout|retrans|syn' || out_line "  （无相关增量）"
  fi
fi

out_line ""
out_line "=== 判读提示（阈值见头部；单轮 ⇒ 只定方向）==="
P=$(peak "$TMP/farm_pmax")
if [ -n "$P" ]; then
  if [ "$P" -ge "$TH_FARM_PCT" ]; then
    out_line "  墙 A 农场：峰值单口 ${P}% ≥ ${TH_FARM_PCT}% ⇒ **成立**（每口事件循环到顶 ⇒ 该加口数，不是加 CPU）"
  else
    out_line "  墙 A 农场：峰值单口 ${P}% < ${TH_FARM_PCT}% ⇒ 本机未见单口打满（更可能是并发/内核侧，而非 CPU 排程）"
  fi
else
  out_line "  墙 A 农场：本机不可见"
fi
K=$(peak "$TMP/k6")
if [ -n "$K" ] && [ "$NPROC" -gt 0 ]; then
  THR=$(awk -v n="$NPROC" -v p="$TH_K6_NPROC_PCT" 'BEGIN{printf "%.1f", n*p/100}')
  if awk -v a="$K" -v b="$THR" 'BEGIN{exit !(a>=b)}'; then
    out_line "  墙 B k6  ：峰值 ${K} 核 ≥ ${THR} 核（${TH_K6_NPROC_PCT}%×nproc）⇒ **成立**（压测端自己就是瓶颈 ⇒ 降采样 / 挪机）"
  else
    out_line "  墙 B k6  ：峰值 ${K} 核 < ${THR} 核（${TH_K6_NPROC_PCT}%×nproc）⇒ 本机未见压测端打满"
  fi
elif [ -n "$K" ]; then
  out_line "  墙 B k6  ：峰值 ${K} 核（nproc 读不到 ⇒ 无法套阈值，自己对一下这台机的核数）"
else
  out_line "  墙 B k6  ：本机不可见"
fi
G=$(peak "$TMP/gw")
if [ -n "$G" ]; then
  THR=$(awk -v t="$TH_GW_TWT" -v p="$TH_GW_TWT_PCT" 'BEGIN{printf "%.2f", t*p/100}')
  if awk -v a="$G" -v b="$THR" 'BEGIN{exit !(a>=b)}'; then
    out_line "  墙 C 网关：峰值 ${G} 核 ≥ ${THR} 核（${TH_GW_TWT_PCT}%×TWT ${TH_GW_TWT}）⇒ **成立**（先按 CONFIG.md §1 调 TWT，再谈改代码）"
  else
    out_line "  墙 C 网关：峰值 ${G} 核 < ${THR} 核（${TH_GW_TWT_PCT}%×TWT ${TH_GW_TWT}）⇒ 本机未见网关打满"
  fi
else
  out_line "  墙 C 网关：本机不可见"
fi
H=$(peak "$TMP/health")
if [ -n "$H" ]; then
  if [ "$H" -ge "$TH_HEALTH_MS" ]; then
    out_line "  /health  ：max ${H}ms ≥ ${TH_HEALTH_MS}ms（HAProxy timeout check 默认取 inter）⇒ 足以解释 503 风暴（backend DOWN）"
  else
    out_line "  /health  ：max ${H}ms < ${TH_HEALTH_MS}ms ⇒ 这一轮没看到健康检查超时"
  fi
fi
out_line ""
out_line "  对齐方式：三台机的行首 [HH:MM:SS] 直接对时间；只有**同一时刻**同时成立的墙才算数。"

rm -rf "$TMP" 2>/dev/null
exit 0
