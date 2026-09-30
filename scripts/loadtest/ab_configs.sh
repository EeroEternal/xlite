#!/bin/bash
# =============================================
# 单二进制配置 A/B（也支持同机多臂）—— 产出「可复现的评测工件」。
#
# 与 run_levels.sh 的分工：
#   run_levels.sh  = 对标 New API 用的**定档位**压测，带报告闸门；
#   ab_configs.sh  = 判**同一份代码里某个旋钮/某个改动**值不值，判决指标是
#                    **网关进程 CPU**（而不只是延迟）。
#
# 为什么判决指标要包含 CPU：低载延迟在 §12 那一轮已经被证明对多数改动不敏感
# （零拷贝 12 项延迟与 CPU 两头都测不出），而 `TOKIO_WORKER_THREADS` 这类旋钮
# 恰恰是"延迟几乎不变、CPU 差 40%"。只看延迟会把这类结论整个漏掉。
#
# 用法：每个「臂 × 配置」组合跑一轮 k6，记录 gw_oh 分位 + 网关 CPU 核数。
#
#   # 同一二进制的线程数扫描（复现 §12.1）
#   ARMS="A2:18101" CONFIGS="TWT=128 TWT=8 TWT=2" scripts/loadtest/ab_configs.sh
#
#   # 两个二进制对比（复现 §12.6：L=RwLock, S=ArcSwap）
#   ARMS="L:18110 S:18111" CONFIGS="TWT=128 TWT=8" scripts/loadtest/ab_configs.sh
#
# 环境变量（都有默认）:
#   GATEWAY_HOST=10.99.255.101        网关机（k6 从本机打过去）
#   GATEWAY_DIR=/home/bodesi/ab6      网关机上的目录：要有 xlite-<arm> 与 <arm>.db
#   ARMS="A2:18101"                   臂 = 二进制名:端口，空格分隔多个
#   CONFIGS="-"                       配置：逗号分隔的 KEY=VAL，空格分隔多组；"-" 表示默认
#                                     可用键：TWT / CONNS / RELOAD
#   VUS=1000 DURATION=120s WARMUP=20 GRACEFUL_STOP=20s
#   K6_DIR=~/labtest/k6dir2 K6_IMAGE=k6-customer:latest K6_MEM=45g
#   TOKEN=sk-loadtest MODEL=mock
#   MOCK_BATCH=<空> MOCK_PORTS="4101-4124"
#                                      mock 的 chunk 合批数（下发给所有口并回读校验）。
#                                      **留空 = 不动农场**（默认，农场由调用方预先配好）。
#                                      注意它**同时改网关 CPU**（batch=10 让网关 7.5→2.6 核），
#                                      所以做臂间比较时所有臂必须同一个值。
#   OUT=~/labtest/ab-configs
#
# ⚠️ 两条必须遵守的台架纪律（都踩过）:
#   1. GRACEFUL_STOP 必须给短值。默认 600s 时，个别上游挂死的请求会把每轮从
#      4 分钟拖到 10.5 分钟（实测）。
#   2. 网关没起来就绝不能开打：脚本会校验拿到数字 pid，失败重试一次后跳过该组合
#      并显式记录，而不是产出一行 100% 失败的假数据（实测踩过）。
#
# 产物（OUT 目录，即"工件"）:
#   progress.log        每组合一行摘要：cores / p50..p99 / qps / fail
#   <arm>-<cfg>.log     k6 原始输出
#   cpu-<arm>-<cfg>.log 每秒一行 `<epoch> <utime> <stime> <nthreads>`（/proc/<pid>/stat）
#
# ⚠️ 判读纪律见 docs/manual/v88-rig.md §9.1：CPU 抖动约 1%，延迟的窗口漂移
#    可达 −3% ~ −9.8%。CPU 差异要大于 ~2% 才算数；延迟差异必须**同窗口交错 ≥3 轮**
#    才能认，n=1 的延迟只能算提示。
# =============================================

set -uo pipefail

GATEWAY_HOST=${GATEWAY_HOST:-10.99.255.101}
GATEWAY_DIR=${GATEWAY_DIR:-/home/bodesi/ab6}
ARMS=${ARMS:-"A2:18101"}
CONFIGS=${CONFIGS:-"-"}
VUS=${VUS:-1000}
DURATION=${DURATION:-120s}
WARMUP=${WARMUP:-20}
GRACEFUL_STOP=${GRACEFUL_STOP:-20s}
K6_DIR=${K6_DIR:-$HOME/labtest/k6dir2}
K6_IMAGE=${K6_IMAGE:-k6-customer:latest}
K6_MEM=${K6_MEM:-45g}
TOKEN=${TOKEN:-sk-loadtest}
MODEL=${MODEL:-mock}
TARGET_HOST=${TARGET_HOST:-$GATEWAY_HOST}
# k6 喂法。**默认 KEEP_BODY=true 保持向后兼容**，但它有一个静默陷阱：
# `gateway_overhead_ms` 只在 KEEP_BODY=true 时注册（k6 要逐字节解析 502 个 chunk），
# 于是 k6 自己会先饱和 —— 实测 4000 VU 下 KB=true 的 qps 封顶 ~315、而网关与农场 CPU
# 双双不再增长，**量到的是 k6 的墙而不是网关的**。判据是"实测 qps ÷ (VUS ÷ dur_p50)"
# （见下面的客户端饱和检测），不是拍脑袋的 VU 阈值。
#   延迟/对标用途 → KEEP_BODY=true（要有 gw_oh）
#   产能/CPU 用途 → KEEP_BODY=false（与 run_levels.sh 的"对标模式"分开跑，见 v88-rig §4）
KEEP_BODY=${KEEP_BODY:-true}
# KEEP_SAMPLE 只影响**脚本解析**的比例（checks 比 2.00 → 1.10），**不影响 k6 是否读完整个流**
# （那由全局 `discardResponseBodies: !KEEP_BODY` 决定）—— 所以 dur/TTFT 在任何一档都有效。
# 三种喂法实测（4000 VU，各 120s）：KB=false → 600.3 qps（无 gw_oh）；
# KB=true SAMPLE=1 → 315.0 / 320.2 qps（全量 gw_oh，k6 最贵，iter_p50 12.2–12.4s）；
# KB=true SAMPLE=0.1 → 603.8 / 604.4 qps（gw_oh 取 10% 样本，仍够出分位）。
# **始终显式传**，不要让 k6.js 的默认值替我们决定喂法：实验室那份 k6.js 默认 0.1，
# 本仓库默认 1 —— 不传就是「同名不同喂法」。实测过这个坑：脚本日志写着 KEEP_BODY=true
# （当时还没转发 KEEP_SAMPLE），实际跑的是 0.1，于是拿到 601 qps 且**看起来一切正常**。
KEEP_SAMPLE=${KEEP_SAMPLE:-1}
# mock 的 chunk 合批数。**留空 = 不动农场配置**（本脚本原来的行为：农场由调用方预先配好）。
# 设成数字就把这一个参数下发给 MOCK_PORTS 的每个口，并**回读校验**。
#   - `batch>1` 是**产能杠杆**：4000 VU 同载荷（字节数与 502 个 chunk 都不变）下
#     `batch=1 → 10` 让农场 22.2→6.8、网关 7.5→2.6、k6 20.0→5.0 核 —— 但注意**网关 CPU 也降**，
#     所以本脚本的判决指标（网关 CPU）同样会被它改掉，做**臂间比较**时必须所有臂用同一个值。
#   - 代价：**首字推迟约 `(batch-1)×21ms`**（实测 500→722ms @batch=10）。延迟类实验留 1。
MOCK_BATCH=${MOCK_BATCH:-}
MOCK_PORTS=${MOCK_PORTS:-"4101-4124"}
OUT=${OUT:-$HOME/labtest/ab-configs}
SSH_OPTS=${SSH_OPTS:--o BatchMode=yes -o StrictHostKeyChecking=no}
GW="ssh $SSH_OPTS $GATEWAY_HOST"

mkdir -p "$OUT"

# 台架互斥 —— 与 run_levels.sh / ab_topology.sh 共用同一把锁（见 lib_rig.sh）
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib_rig.sh
. "$SCRIPT_DIR/lib_rig.sh"
lab_lock "ab_configs arms=$ARMS" || exit 1

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/progress.log"; }

# ---------------------------------------------------------------- 收尾（在**任何子进程之前**注册）
# ⚠️ 本脚本原来**没有**任何 trap：网关只在正常路径上被 `ab_gw.sh stop` 关掉，所以 Ctrl-C /
# kill / 中途 `exit` 都会留下**孤儿网关 + k6 容器**，而且**台架锁也释放不掉** ——
# `docker run` 客户端与 `$GW` 起的 ssh 子进程都**继承了 fd 9**（实测：kill 掉本脚本后
# `flock -n` 仍然 BLOCKED，得手工找出持锁 PID 才解开）。这与 `ab_topology.sh`
# §13.6 第 0b 条是同一类问题，当时没修这个脚本。
#
# 两条纪律，**第 2 条才是关键**：
#   1) `trap cleanup EXIT` 必须在启动任何子进程之前注册（否则中途 die 时从不执行）；
#   2) **前台跑的子进程会把 trap 推迟到它结束之后** —— bash 只在当前前台命令结束后才
#      处理已到达的信号。所以 `run_k6` 必须**后台跑 + `wait`**（`wait` 可被信号立即打断）。
#      实测：前台 `docker run` 期间发 TERM，30s 后脚本仍活着、网关仍在、锁仍被占；
#      改成后台 + `wait` 后 10s 内全部释放。
#   ⚠️ 顺带纠正一个我先前写错的断言：**只挂 `EXIT` 就已经覆盖 SIGINT/SIGTERM** ——
#      实测 `trap cleanup EXIT`（不挂 INT/TERM）在 `wait` 期间收 TERM/INT，cleanup 都会跑
#      （rc 143/129，0.01s），与额外挂 `trap 'exit 130' INT` 的延迟相同。所以**不需要**
#      那两个 trap；真正的修复是上面第 2 条。
#   3) 变过农场配置就要**还原**（见下面 `FARM_SAVED`）：24 个 mock 是**共享**的，
#      把 `batch=10` 留在那里会让下一个"不动农场"的 session 静默量到相反的口径。
CUR_PORT=""
K6_PID=""
# 逐口记原值：农场可能**各口不一致**（别人只改了一部分），所以还原必须按口来，
# 不能拿"第一个读到的值"回写所有口（那会静默抹掉别的口的值）。
# 用 lib_rig.sh 的 `farm_remember` 记在一条 `port=batch` 串里 —— **不用关联数组**：
# `declare -A` 是 bash 4+，而 macOS 自带的 bash 是 3.2（见 lib_rig.sh 的说明）。
FARM_SAVED=""
cleanup() {
  trap - EXIT
  [ -n "$K6_PID" ] && kill "$K6_PID" 2>/dev/null
  docker rm -f ab_configs_k6 >/dev/null 2>&1
  [ -n "$CUR_PORT" ] && $GW "bash /tmp/ab_gw.sh stop '$CUR_PORT'" >/dev/null 2>&1
  # 还原农场：**按口**回写，且只要**曾经写过**就还原 —— 不押在"最后验成功了"上。
  # （守卫失败/有口读不到时写入早已发生，那时不还原就会把 24 个 mock 永久留在台架口径上；
  #   这正是复评第 2 轮指出的洞。）
  if [ -n "$FARM_SAVED" ]; then
    local n=0 kv
    for kv in $FARM_SAVED; do
      curl -s -m 3 -X POST "http://127.0.0.1:${kv%%=*}/_config?batch=${kv#*=}" >/dev/null 2>&1
      n=$((n + 1))
    done
    log "  mock _config: 已按口还原 batch（$n 口）"
  fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- 农场配置（拿锁之后）
# `_config` 是**静默**的：参数名写错不报错，只会把「我以为我设了 batch」变成假结论。
# 所以：**先逐口回读**（无论改不改都写进日志 —— 否则工件里看不出这一轮跑的是什么口径），
# 校验格式，下发，再回读校验，并在收尾**按口**还原。
MOCK_PORTS_LIST=$(expand_ports "$MOCK_PORTS")
n_ports=0
for p in $MOCK_PORTS_LIST; do n_ports=$((n_ports + 1)); done
# ⚠️ 必须有这一条：`MOCK_PORTS=','` 之类会展开成**空白**，循环 0 次，于是下面
# `n_ok -eq n_ports` 是 **0 == 0 空转通过**（复评第 2 轮指出：我上一版只在注释里
# 声称有这道守卫，代码里并没有）。
[ "$n_ports" -gt 0 ] || { printf '✗ MOCK_PORTS=%s 展开后一个端口都没有（支持 "4101-4124" 或 "4101,4102"）\n' "$MOCK_PORTS" >&2; exit 2; }

n_before=0; seen_batches=""
for p in $MOCK_PORTS_LIST; do
  b=$(curl -s -m 3 -X POST "http://127.0.0.1:$p/_config" 2>/dev/null | grep -o '"batch":[0-9]*}' | tr -dc '0-9')
  if [ -n "$b" ]; then
    n_before=$((n_before + 1))
    FARM_SAVED=$(farm_remember "$FARM_SAVED" "$p" "$b")
    case " $seen_batches " in *" $b "*) ;; *) seen_batches="$seen_batches $b" ;; esac
  fi
done
seen_batches=${seen_batches# }
log "农场现状: batch=[${seen_batches:-unknown}]（$n_before/$n_ports 口读到）$([ "$n_before" -eq "$n_ports" ] || echo '  ⚠️ 有口没读到')"
# 没让我们改，但农场不在客户基线 1 上 ⇒ **必须说出来**，否则下一轮会静默量到别的口径。
# **不要**把这条押在"所有口都读到"上：恰恰是有口读不到时最容易留下脏状态（复评第 2 轮）。
if [ -z "${MOCK_BATCH}" ] && [ -n "$seen_batches" ] && [ "$seen_batches" != "1" ]; then
  log "  ⚠️ 农场 batch=[$seen_batches] 不是客户基线 1 —— 这一轮是**台架口径**，不能与对标数据混比（§14）"
fi

if [ -n "${MOCK_BATCH}" ]; then
  case "${MOCK_BATCH}" in
    ''|*[!0-9]*) printf '✗ MOCK_BATCH 必须是十进制整数（拿到 %s）\n' "'$MOCK_BATCH'" >&2; exit 2 ;;
  esac
  # **只写原值已知的口**：这样"写过 ⇒ 一定能还原"是不变量（复评第 3 轮 minor 5：
  # 旧写法写遍所有口，而只有预读成功的口记了原值 ⇒ "预读失败、随后又能写"的口改不掉回）。
  # 读不到的口由下面 `n_ok -eq n_ports` 的守卫 fail-closed 兜住。
  for kv in $FARM_SAVED; do
    curl -s -m 3 -X POST "http://127.0.0.1:${kv%%=*}/_config?batch=${MOCK_BATCH}" >/dev/null 2>&1
  done
  n_ok=0
  for p in $MOCK_PORTS_LIST; do
    case "$(curl -s -m 3 -X POST "http://127.0.0.1:$p/_config" 2>/dev/null)" in
      *"\"batch\":$MOCK_BATCH}"*) n_ok=$((n_ok + 1)) ;;
    esac
  done
  log "  mock _config: batch=${MOCK_BATCH}（回读生效 $n_ok/$n_ports 口）"
  # 收尾**无条件**还原（写已经发生了）；失败时更要还原，所以这条在 exit 之前
  [ "$n_ok" -eq "$n_ports" ] || { log "  ✗ batch=$MOCK_BATCH 未在所有 mock 上生效（$n_ok/${n_ports}）—— 静默失效会产出假结论"; exit 2; }
fi

md5_of() {
  if command -v md5sum >/dev/null 2>&1; then md5sum "$1" | awk '{print $1}';
  else md5 -q "$1"; fi
}
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# k6 的耗时是 **Go duration 字面量**，不是"若干秒"：
#   `37.83µs` / `3.84ms` / `5.89s` / `1m4s` / `1h2m3s`（仓内真实日志里都出现过，
#   其中 `http_req_duration` 行上的分钟级 p50（`1m4s`…`3m5s`）有 15 处 —— 正是**卡住**的那些轮次）。
# 所以不能只 `sub(/s$/,"")` 当秒数用：`1m4s` 会变成 `1m4`、`2.29ms` 变成 `2.29m`。
# 本函数把字面量严格换算成**秒**；**完全消费不掉就返回空**（调用方据此跳过判定），
# 不要让 awk 把垃圾字符串静默当 0。
secs_of() {
  # 微秒号 U+00B5（UTF-8 = C2 B5）先换成 ASCII `u`，**必须在 bash 里换**：
  # macOS 自带的 BWK awk（`awk version 20200816`）**在 UTF-8 locale 下把多字节字面量
  # `"µs"` 与 `"s"` 判为相等** —— 实测 `("µs" == "s")` 在 `LC_ALL=en_US.UTF-8` 下是 **1**、
  # 在 `LC_ALL=C` 下才是 **0**。于是单位判定**两个方向都会错**：
  #   `unit == "µs"` 对 `"s"` 判真 ⇒ `5.89s` 走微秒那一支，算成 5.89e-06（`1m4s` 同病）；
  #   `unit == "s"`  对 `"µs"` 判真 ⇒ `37.83µs` 走秒那一支，算成 37.83s（**137 倍**）。
  # ⚠️ **只有字符串比较有此问题，正则字面量是好的**：`("37.83µs" ~ /µs$/)` 在两种 locale 下
  # 都正确（所以别把这句读成"正则也会被截断"—— 那是错的，已实测）。
  # 换成 ASCII 之后 awk 只面对 ASCII，行为与 locale、与 awk 实现（BWK / gawk / mawk）都无关。
  local t=${1//$'\xc2\xb5'/u}
  awk -v t="$t" 'BEGIN{
    if (t ~ /^[0-9]+(\.[0-9]+)?$/) { printf "%.9g", t + 0; exit }   # 裸数字按秒收
    s = t; total = 0; ok = 0
    while (s != "" && match(s, /^[0-9]+(\.[0-9]+)?(ns|us|ms|s|m|h)/)) {
      tok = substr(s, 1, RLENGTH)
      unit = tok; sub(/^[0-9.]+/, "", unit)
      num = tok;  sub(/(ns|us|ms|s|m|h)$/, "", num)
      if (unit == "ns") mult = 1e-9
      else if (unit == "us") mult = 1e-6
      else if (unit == "ms") mult = 1e-3
      else if (unit == "s") mult = 1
      else if (unit == "m") mult = 60
      else mult = 3600
      total += num * mult; ok = 1
      s = substr(s, RLENGTH + 1)
    }
    if (ok && s == "") printf "%.9g", total
  }'
}

# 只接受十进制数字串（至多一个小数点），否则返回空。挡 `abc` / `1.2.3` / `1e3` 这类
# 会被 awk 的 `+0` 静默吞掉的输入 —— `qps` 与耗时都要过这道闸。
num_of() {
  awk -v v="$1" 'BEGIN{ if (v ~ /^[0-9]+(\.[0-9]+)?$/) print v }'
}

# 配置短键 → 网关**真实** env 名。
# 不映射就会把 `TWT=128` 原样丢给 `env`，而网关只读 TOKIO_WORKER_THREADS —— 那会静默
# 产出「三臂完全相同 = 零效应」的假结论（这条正是评审抓到的 blocker）。
map_cfg_key() {
  case "$1" in
    TWT)    printf 'TOKIO_WORKER_THREADS' ;;
    CONNS)  printf 'XLITE_UPSTREAM_MAX_CONNS_PER_HOST' ;;
    RELOAD) printf 'XLITE_CATALOG_RELOAD_SECS' ;;
    *) return 1 ;;
  esac
}

# 跑数身份：k6.js 与 k6 镜像必须可追溯，否则「工件可复跑」是空话。
# 容器里跑的就是挂载点 `/k6/k6.js`，所以指纹只能取这一份（不做覆盖，免得多一个能对不上的旋钮）。
K6_JS="$K6_DIR/k6.js"
[ -f "$K6_JS" ] || { echo "✗ 找不到 ${K6_JS}（k6 脚本；它是以 -v ${K6_DIR}:/k6 挂进容器的）" >&2; exit 2; }
K6_JS_MD5=$(md5_of "$K6_JS")
K6_JS_SHA256=$(sha256_of "$K6_JS")
# digest 优先，取不到就退回镜像 ID；两种情况都可能只吐一个空行，所以必须显式判空。
K6_IMAGE_ID=$(docker image inspect --format '{{index .RepoDigests 0}}' "$K6_IMAGE" 2>/dev/null | head -1 | tr -d '\n' || true)
[ -n "$K6_IMAGE_ID" ] || K6_IMAGE_ID=$(docker image inspect --format '{{.Id}}' "$K6_IMAGE" 2>/dev/null | head -1 | tr -d '\n' || true)
[ -n "$K6_IMAGE_ID" ] || K6_IMAGE_ID=n/a
K6_GRACEFUL_OK=$(grep -c 'gracefulStop: GRACEFUL_STOP' "$K6_JS" || true)
[ "$K6_GRACEFUL_OK" -ge 1 ] || { echo "✗ $K6_JS 不是仓库版（gracefulStop 未参数化）" >&2; exit 2; }

# 配置键**和值**开跑前就校验：拼错一个键/写个非法值必须在这里死，而不是等 ssh/docker
# 起来之后才发现「三臂跑的是同一个默认值」。网关对非法 env 会**静默**退回默认
# （`src/config.rs` 的 `unwrap_or(32)` / `unwrap_or(5)`），那正是假零效应的成因。
for cfg in $CONFIGS; do
  [ "$cfg" = "-" ] && continue
  for kv in $(printf '%s' "$cfg" | tr ',' ' '); do
    key=${kv%%=*}; val=${kv#*=}
    [ "$key" = "$kv" ] && { echo "✗ 配置 '$kv' 缺少 '=值'（形如 TWT=128）" >&2; exit 2; }
    map_cfg_key "$key" >/dev/null || {
      echo "✗ 未知配置键 '$key'（可用：TWT / CONNS / RELOAD）—— 原样传给 env 会静默产出假零效应" >&2
      exit 2
    }
    case "$val" in
      ''|*[!0-9]*) echo "✗ 配置 '$kv' 的值必须是十进制整数（非数字会被网关静默丢掉，退回默认）" >&2; exit 2 ;;
    esac
    # 位数先卡一道：超大数字直接过 `[ -le ]` 会让 `[` 自己报 integer expected 噪音。
    if [ "${#val}" -gt 5 ]; then
      echo "✗ 配置 '$kv' 位数超出（≤ 5 位）—— 超范围的值要么被网关静默退回默认，要么让 tokio panic" >&2
      exit 2
    fi
    case "$key" in
      # tokio 对非法/0 的 TOKIO_WORKER_THREADS 是**启动即 panic**（不是静默退回）；上界只是策略上限。
      TWT)    [ "$val" -ge 1 ] && [ "$val" -le 65535 ] || { echo "✗ 配置 '$kv' 非法（tokio 对非法或 0 值会 panic；上界 65535 为策略上限）" >&2; exit 2; } ;;
      # 这两个是 `unwrap_or` 静默退回默认：usize 解析失败 → 32 / 5。
      CONNS)  [ "$val" -ge 1 ] && [ "$val" -le 65535 ] || { echo "✗ 配置 '$kv' 非法或 > 65535（单目的地无意义；解析失败会被网关静默退回 32）" >&2; exit 2; } ;;
      RELOAD) [ "$val" -le 86400 ] || { echo "✗ 配置 '$kv' 超出上界 86400（= 1 天；再大等同于永不重载）" >&2; exit 2; } ;;
    esac
  done
done

# ---------------------------------------------------------------- 远端助手
# 装一次，之后用参数调用，避免多层引号转义（这里踩过好几次）。
$GW "cat > /tmp/ab_gw.sh" <<'REMOTE'
#!/bin/bash
# ab_gw.sh start <dir> <arm> <port> [KEY=VAL ...] | cpu <pid> <out> | stop <port>
case "$1" in
  start)
    dir="$2"; arm="$3"; port="$4"; shift 4
    pid=$(ss -lntp 2>/dev/null | grep ":$port " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
    [ -n "$pid" ] && kill "$pid" 2>/dev/null && sleep 2
    cd "$dir" || exit 1
    # 网关继承本 shell 的 nofile；ssh 非交互默认只有 1024，会把高 VU 档静默卡住
    ulimit -n 1048576 2>/dev/null || true
    env DATABASE_URL="sqlite:$dir/$arm.db" PORT="$port" LISTEN_HOST=0.0.0.0 \
        XLITE_UPSTREAM_H2C=1 "$@" \
      setsid nohup "$dir/xlite-$arm" > "$dir/gw-$arm-$port.log" 2>&1 < /dev/null &
    for _ in $(seq 1 30); do
      sleep 1; curl -s -m 3 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && break
    done
    pid=$(ss -lntp 2>/dev/null | grep ":$port " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
    bl=$(ss -lnt "sport = :$port" 2>/dev/null | awk 'NR>1{print $3; exit}')
    nofile=$(awk '/Max open files/{print $4}' "/proc/$pid/limits" 2>/dev/null)
    # 网关继承本 shell 的 nofile：ssh 默认 1024 会把高 VU 档**静默**卡住，
    # 表现是"两臂无差异"这种假结论。所以这里也硬判，别只 echo。
    case "$nofile" in ''|*[!0-9]*) : ;; *) [ "$nofile" -lt 65536 ] && { echo "START_FAILED nofile=$nofile 太小"; exit 1; } ;; esac
    echo "pid=$pid backlog=$bl nofile=$nofile health=$(curl -s -m3 "http://127.0.0.1:$port/health")"
    ;;
  cpu)
    pid="$2"; out="$3"; : > "$out"
    while kill -0 "$pid" 2>/dev/null; do
      [ -r "/proc/$pid/stat" ] && awk -v t="$(date +%s)" '{print t, $14, $15, $20}' "/proc/$pid/stat" >> "$out"
      sleep 1
    done
    ;;
  stop)
    pid=$(ss -lntp 2>/dev/null | grep ":$2 " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
    [ -n "$pid" ] && kill "$pid" 2>/dev/null && echo "stopped $2" || echo "nothing on $2"
    ;;
esac
REMOTE
$GW "chmod +x /tmp/ab_gw.sh"

# CORES=增量 utime+stime ÷ 墙钟（100 Hz tick）
cores_of() {
  $GW "awk 'NR==1{t0=\$1;u0=\$2;s0=\$3} END{if(NR>1) printf \"%.3f\", ((\$2+\$3)-(u0+s0))/100/(\$1-t0); else print \"n/a\"}' $1"
}

start_arm() {   # $1=arm $2=port $3...=env；成功则打印远端 info，失败打印 START_FAILED
  local arm="$1" port="$2"; shift 2
  local info pid
  for _ in 1 2; do
    info=$($GW "bash /tmp/ab_gw.sh start '$GATEWAY_DIR' '$arm' '$port' $*")
    pid=$(printf '%s' "$info" | sed -E 's/.*pid=([0-9]+).*/\1/')
    case "$pid" in ''|*[!0-9]*) sleep 4 ;; *) printf '%s' "$info"; return 0 ;; esac
  done
  printf 'START_FAILED'; return 1
}

run_k6() {      # $1=port $2=outfile
  # **永远显式传**：早先"默认 1 时省略"导致日志写 KEEP_SAMPLE=1 而实际跑的是 k6.js
  # 自己的默认值（实验室那份是 0.1）—— 日志与实际不一致，正是最难查的一类问题。
  # `--name` 固定名字是**为了让 cleanup 能确定性地删掉它**：`--rm` 只保证容器正常退出时自删，
  # 脚本被 Ctrl-C / kill 时容器会留下（pip 里也能看到无名的 k6-customer 容器）。
  # ⚠️ **必须后台跑 + `wait`，不能前台跑**：bash 只在**当前前台命令结束之后**才处理已到达的
  # 信号，所以前台 `docker run` 期间收到 TERM，trap 会一直拖到 k6 跑完（实测 300s 档被 TERM
  # 后 30s：脚本仍活着、网关仍在、**台架锁仍被占**）。而 `wait` 可被信号立即打断 —— trap
  # 随即执行，cleanup 就能**当场**删容器、关网关、把锁放掉。
  # `9>&-`：**后台子进程绝不能继承锁 fd**（lib_rig.sh 的约定），否则它替脚本继续持锁。
  # 先删同名残留：上一轮被 `kill -9` 时留下的容器会让 `docker run` 直接拒绝（名字冲突）。
  docker rm -f ab_configs_k6 >/dev/null 2>&1
  # `--ulimit nofile`：k6 是这条链路上**唯一**要从一台机同时持住大量 socket 的进程，不设时
  # 它继承 docker daemon 的默认（可能低到 1024 / 65536）⇒ 连接接不完或持不住。同因的修复见
  # deploy/v88/run_full_88_xg.sh 的 run_k6_once（#43）。
  docker run --rm --name ab_configs_k6 -m "$K6_MEM" \
    --ulimit nofile=1048576:1048576 \
    --network=host -v "$K6_DIR:/k6" \
    -e TARGET_URL="http://$TARGET_HOST:$1/v1/chat/completions" \
    -e TOKEN="$TOKEN" -e MODEL="$MODEL" -e STREAM=true \
    -e IN_TOKENS=100 -e OUT_TOKENS=500 -e VUS="$VUS" -e DURATION="$DURATION" -e MAX_VUS="$VUS" \
    -e STREAM_TIMEOUT=600000 -e WARMUP="$WARMUP" -e "KEEP_BODY=$KEEP_BODY" \
    -e "KEEP_SAMPLE=$KEEP_SAMPLE" \
    -e GRACEFUL_STOP="$GRACEFUL_STOP" \
    "$K6_IMAGE" run --compatibility-mode=base \
    --summary-trend-stats='p(50),p(90),p(95),p(99),avg,min,max' /k6/k6.js > "$2" 2>&1 9>&- &
  K6_PID=$!
  wait "$K6_PID"
  K6_PID=""
}

# ---------------------------------------------------------------- 主循环
log "ab_configs: arms=[$ARMS] configs=[$CONFIGS] vus=$VUS dur=$DURATION stop=$GRACEFUL_STOP"
log "喂法: KEEP_BODY=$KEEP_BODY KEEP_SAMPLE=${KEEP_SAMPLE}（KB=true 才有 gateway_overhead_ms；KB=false 用于产能/CPU）"
log "身份: k6.js=$K6_JS md5=$K6_JS_MD5 sha256=$K6_JS_SHA256 | k6 镜像=$K6_IMAGE id=$K6_IMAGE_ID"
# 打印那份 k6.js **自己的** KEEP_SAMPLE 默认值：实验室目录里可能是客户 memopt 版（0.1），
# 与仓库版（1）不同 —— 不显式传就会被它悄悄决定喂法。
log "k6.js 自带默认: $(grep -m1 -o 'var KEEP_SAMPLE = parseFloat(__ENV.KEEP_SAMPLE || .[0-9.]*.)' "$K6_JS" 2>/dev/null || echo '(未找到 var KEEP_SAMPLE)')"
ok_groups=0
for arm_spec in $ARMS; do
  arm=${arm_spec%%:*}; port=${arm_spec##*:}
  arm_hash=$($GW "sha256sum '$GATEWAY_DIR/xlite-$arm' 2>/dev/null | awk '{print \$1}'" || true)
  log "臂 $arm 二进制 sha256=${arm_hash:-unknown}"
  for cfg in $CONFIGS; do
    if [ "$cfg" = "-" ]; then
      cfg_env=""; label="$arm-default"
    else
      cfg_env=""
      for kv in $(printf '%s' "$cfg" | tr ',' ' '); do
        key=${kv%%=*}; val=${kv#*=}
        real=$(map_cfg_key "$key")   # 键与值都已在开跑前校验过
        cfg_env="$cfg_env $real=$val"
      done
      label="$arm-$(printf '%s' "$cfg" | tr ',' '_')"
    fi
    log "开始 $label (port $port) 生效 env:[${cfg_env:- 默认}]"
    # 记下**正在跑的口**：cleanup 只关这一个，不去碰别人 session 的网关
    CUR_PORT="$port"
    info=$(start_arm "$arm" "$port" $cfg_env)
    # ⚠️ START_FAILED **不能**清 CUR_PORT：`ab_gw.sh start` 会**先起进程**再判 nofile
    # （§13.6 0a 那条硬断言），所以这条路径上可能已经留下一个占着端口的网关 ——
    # 清了 CUR_PORT 就等于放弃清理它。让它留着，交给 cleanup 去 stop。
    if [ "$info" = "START_FAILED" ]; then log "  $label 网关启动失败，跳过（交给 cleanup 收口）"; continue; fi
    log "  gw: $info"
    pid=$(printf '%s' "$info" | sed -E 's/.*pid=([0-9]+).*/\1/')
    gw_backlog=$(printf '%s' "$info" | sed -nE 's/.*backlog=([0-9]+).*/\1/p')
    [ -n "$gw_backlog" ] || log "  ⚠️ $label 读不到监听 backlog（远端没有 ss？）—— accept 触顶无法判定"
    $GW "setsid nohup bash /tmp/ab_gw.sh cpu '$pid' '$GATEWAY_DIR/cpu-$label.log' >/dev/null 2>&1 < /dev/null & sleep 1; echo up" >/dev/null
    run_k6 "$port" "$OUT/$label.log"
    $GW "bash /tmp/ab_gw.sh stop '$port'" >/dev/null 2>&1
    CUR_PORT=""
    cores=$(cores_of "$GATEWAY_DIR/cpu-$label.log")
    log "  cores=$cores backlog=${gw_backlog:-unknown} env:[${cfg_env:- 默认}]"
    grep -E "gateway_overhead_ms|http_req_duration\.|iteration_duration\.|http_reqs\.|http_req_failed" "$OUT/$label.log" \
      | sed "s/^/  $label /" | tee -a "$OUT/progress.log"

    # ---- 客户端饱和检测：**把静默陷阱变成可见** ----
    # 分母必须是**服务时间**（`http_req_duration` p50），不是迭代周期
    # （`iteration_duration` p50）—— 后者**已经包含客户端自己的空档**，拿它算比值会恒等于
    # ~100%，检测器永远不响（我自己踩过：4000 VU 下 dur=5.85s 而 iter=12.23s，
    # 用 iter 算得 96% 于是漏报，真实比值是 315/(4000/5.85)=46%）。
    # 理论速率 = VUS ÷ 服务时间（闭式负载的稳态上限）。实测远低于它 ⇒ 流不是被网关/农场
    # 处理的，而是 **k6 自己推不动** ⇒ 那一档的 CPU 与延迟都不能用来评判网关。
    # 提取 → 换算 → 闸门。**耗时是 Go duration 字面量**（`5.89s` / `1m4s` / `3.84ms` /
    # `37.83µs`），必须过 `secs_of` 换算成秒；`qps` 过 `num_of` 收成纯数字。
    # 两个 helper 都是"严格全消费，否则返回空"，所以：
    #   - `dur=abc` / `1.2.3` / 空 ⇒ 空 ⇒ **整块跳过**（不会让 awk 把垃圾当 0，
    #     从而凭空报一个「客户端疑似饱和（0%）」）；
    #   - `dur=1m4s` ⇒ **64**（此前只 `sub(/s$/,"")` 会得到 `1m4`，再被闸门吞掉 ⇒
    #     检测器**一声不响地失效**，而分钟级 p50 恰恰就是"卡住"的那一轮）。
    qps=$(num_of "$(awk '/^ *http_reqs\./{for(i=1;i<=NF;i++) if($i ~ /\/s$/){sub(/\/s$/,"",$i); print $i}}' "$OUT/$label.log" | tail -1)")
    dur=$(secs_of "$(awk '/^ *http_req_duration\./{for(i=1;i<=NF;i++) if($i ~ /^p\(50\)=/){sub(/p\(50\)=/,"",$i); print $i}}' "$OUT/$label.log" | tail -1)")
    it=$(secs_of "$(awk '/^ *iteration_duration\./{for(i=1;i<=NF;i++) if($i ~ /^p\(50\)=/){sub(/p\(50\)=/,"",$i); print $i}}' "$OUT/$label.log" | tail -1)")
    if [ -n "$qps" ] && [ -n "$dur" ] && awk -v d="$dur" 'BEGIN{exit !(d+0>0)}'; then
      # `theo` 取整后可能为 0（VUS 小 / dur 极大，例如 `VUS=200` 配 `dur=10m0s`）—— 那一步的
      # `100*q/t` 会除零并吐一个空 ratio，告警被**静默跳过**。这种情况**明说无法判定**。
      theo=$(awk -v v="$VUS" -v d="$dur" 'BEGIN{printf "%.0f", v/d}')
      if [ "$theo" -le 0 ]; then
        log "  （客户端饱和无法判定：$VUS ÷ ${dur}s 取整为 ${theo}，跳过本档判定）"
      else
        ratio=$(awk -v q="$qps" -v t="$theo" 'BEGIN{printf "%.0f", 100*q/t}')
        # 空档是**附加信息**，不影响判定：`it` 已经过 `secs_of`（单位与 dur 一致），
        # 所以这里的减法在 `iter=1m4s` 这类分钟级输入上也是对的。
        gap=""
        if [ -n "$it" ]; then
          gap="；客户端每迭代空档 $(awk -v i="$it" -v d="$dur" 'BEGIN{printf "%.1f", i-d}')s（iter ${it}s vs dur ${dur}s）"
        fi
        log "  客户端占用: 实测 $(awk -v q="$qps" 'BEGIN{printf "%.1f", q}')/s ÷ 理论 ${theo}/s = ${ratio}%${gap}"
        if [ "$ratio" -lt 60 ]; then
          log "  ⚠️ $label 客户端疑似饱和（${ratio}%）：这一档量到的是 **k6 的墙**，不能用来评判网关。"
          log "     → 要 gw_oh 又要高载：KEEP_BODY=true KEEP_SAMPLE=0.1（实测 604 qps vs SAMPLE=1 的 315）；纯产能/CPU：KEEP_BODY=false。"
        fi
      fi
    fi

    # 「成功」必须是**取到指标**，不只是网关起来：docker/k6 全挂时 k6 日志里没有任何指标，
    # 那种组合不能算数（否则会打印「1 个组合成功」并 exit 0）。
    # 指标随模式而变：KB=true 出 gw_oh，KB=false 没有它 —— 后者要有 http_reqs 才算数。
    if [ -n "$cores" ] && [ "$cores" != "n/a" ] && {
         { [ "$KEEP_BODY" = true ] && grep -q 'gateway_overhead_ms' "$OUT/$label.log" 2>/dev/null; } ||
         { [ "$KEEP_BODY" != true ] && grep -qE 'http_reqs\.' "$OUT/$label.log" 2>/dev/null; }
       }; then
      ok_groups=$((ok_groups + 1))
    else
      log "  ✗ $label 未取到有效指标（cores=${cores}）—— 不计入成功"
    fi
  done
done
if [ "$ok_groups" -eq 0 ]; then
  log "✗ ab_configs 结束：0 个组合成功（网关未起/ssh 不通/docker 拉不到？）→ 非零退出，避免把空结果当成结论"
  exit 2
fi
log "ab_configs 完成：$ok_groups 个组合成功"
