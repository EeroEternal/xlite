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
OUT=${OUT:-$HOME/labtest/ab-configs}
SSH_OPTS=${SSH_OPTS:--o BatchMode=yes -o StrictHostKeyChecking=no}
GW="ssh $SSH_OPTS $GATEWAY_HOST"

mkdir -p "$OUT"

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/progress.log"; }

md5_of() {
  if command -v md5sum >/dev/null 2>&1; then md5sum "$1" | awk '{print $1}';
  else md5 -q "$1"; fi
}
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}';
  else shasum -a 256 "$1" | awk '{print $1}'; fi
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
K6_JS=${K6_JS:-$K6_DIR/k6.js}
[ -f "$K6_JS" ] || { echo "✗ 找不到 K6_JS=${K6_JS}（k6 脚本；它是以 -v ${K6_DIR}:/k6 挂进容器的）" >&2; exit 2; }
K6_JS_MD5=$(md5_of "$K6_JS")
K6_JS_SHA256=$(sha256_of "$K6_JS")
K6_IMAGE_ID=$(docker image inspect --format '{{index .RepoDigests 0}}' "$K6_IMAGE" 2>/dev/null \
  || docker image inspect --format '{{.Id}}' "$K6_IMAGE" 2>/dev/null || echo n/a)
K6_GRACEFUL_OK=$(grep -c 'gracefulStop: GRACEFUL_STOP' "$K6_JS" || true)
[ "$K6_GRACEFUL_OK" -ge 1 ] || { echo "✗ $K6_JS 不是仓库版（gracefulStop 未参数化）" >&2; exit 2; }

# 配置键**开跑前**就校验一遍：拼错一个键就必须在这里死，而不是等 ssh/docker 起来之后
# 才发现「三臂跑的是同一个默认值」。
for cfg in $CONFIGS; do
  [ "$cfg" = "-" ] && continue
  for kv in $(printf '%s' "$cfg" | tr ',' ' '); do
    key=${kv%%=*}
    [ "$key" = "$kv" ] && { echo "✗ 配置 '$kv' 缺少 '=值'（形如 TWT=128）" >&2; exit 2; }
    map_cfg_key "$key" >/dev/null || {
      echo "✗ 未知配置键 '$key'（可用：TWT / CONNS / RELOAD）—— 原样传给 env 会静默产出假零效应" >&2
      exit 2
    }
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
    env DATABASE_URL="sqlite:$dir/$arm.db" PORT="$port" LISTEN_HOST=0.0.0.0 \
        XLITE_UPSTREAM_H2C=1 "$@" \
      setsid nohup "$dir/xlite-$arm" > "$dir/gw-$arm-$port.log" 2>&1 < /dev/null &
    for _ in $(seq 1 30); do
      sleep 1; curl -s -m 3 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && break
    done
    pid=$(ss -lntp 2>/dev/null | grep ":$port " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
    bl=$(ss -lnt "sport = :$port" 2>/dev/null | awk 'NR>1{print $3; exit}')
    echo "pid=$pid backlog=$bl health=$(curl -s -m3 "http://127.0.0.1:$port/health")"
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
  docker run --rm -m "$K6_MEM" --network=host -v "$K6_DIR:/k6" \
    -e TARGET_URL="http://$TARGET_HOST:$1/v1/chat/completions" \
    -e TOKEN="$TOKEN" -e MODEL="$MODEL" -e STREAM=true \
    -e IN_TOKENS=100 -e OUT_TOKENS=500 -e VUS="$VUS" -e DURATION="$DURATION" -e MAX_VUS="$VUS" \
    -e STREAM_TIMEOUT=600000 -e WARMUP="$WARMUP" -e KEEP_BODY=true \
    -e GRACEFUL_STOP="$GRACEFUL_STOP" \
    "$K6_IMAGE" run --compatibility-mode=base \
    --summary-trend-stats='p(50),p(90),p(95),p(99),avg,min,max' /k6/k6.js > "$2" 2>&1
}

# ---------------------------------------------------------------- 主循环
log "ab_configs: arms=[$ARMS] configs=[$CONFIGS] vus=$VUS dur=$DURATION stop=$GRACEFUL_STOP"
log "身份: k6.js=$K6_JS md5=$K6_JS_MD5 sha256=$K6_JS_SHA256 | k6 镜像=$K6_IMAGE id=$K6_IMAGE_ID"
ok_groups=0
for arm_spec in $ARMS; do
  arm=${arm_spec%%:*}; port=${arm_spec##*:}
  for cfg in $CONFIGS; do
    if [ "$cfg" = "-" ]; then
      cfg_env=""; label="$arm-default"
    else
      cfg_env=""
      for kv in $(printf '%s' "$cfg" | tr ',' ' '); do
        key=${kv%%=*}; val=${kv#*=}
        if [ "$key" = "$val" ]; then log "✗ 配置 '$kv' 缺少 '=值'（形如 TWT=128）—— 中止"; exit 2; fi
        if ! real=$(map_cfg_key "$key"); then
          log "✗ 未知配置键 '$key'（可用：TWT / CONNS / RELOAD）—— 中止：原样传给 env 会静默产出假零效应"
          exit 2
        fi
        cfg_env="$cfg_env $real=$val"
      done
      label="$arm-$(printf '%s' "$cfg" | tr ',' '_')"
    fi
    log "开始 $label (port $port) 生效 env:[${cfg_env:- 默认}]"
    info=$(start_arm "$arm" "$port" $cfg_env)
    if [ "$info" = "START_FAILED" ]; then log "  $label 网关启动失败，跳过"; continue; fi
    log "  gw: $info"
    pid=$(printf '%s' "$info" | sed -E 's/.*pid=([0-9]+).*/\1/')
    gw_backlog=$(printf '%s' "$info" | sed -nE 's/.*backlog=([0-9]+).*/\1/p')
    [ -n "$gw_backlog" ] || log "  ⚠️ $label 读不到监听 backlog（远端没有 ss？）—— accept 触顶无法判定"
    $GW "setsid nohup bash /tmp/ab_gw.sh cpu '$pid' '$GATEWAY_DIR/cpu-$label.log' >/dev/null 2>&1 < /dev/null & sleep 1; echo up" >/dev/null
    run_k6 "$port" "$OUT/$label.log"
    $GW "bash /tmp/ab_gw.sh stop '$port'" >/dev/null 2>&1
    log "  cores=$(cores_of "$GATEWAY_DIR/cpu-$label.log") backlog=${gw_backlog:-unknown} env:[${cfg_env:- 默认}]"
    grep -E "gateway_overhead_ms|http_reqs\.|http_req_failed" "$OUT/$label.log" | sed "s/^/  $label /" | tee -a "$OUT/progress.log"
    ok_groups=$((ok_groups + 1))
  done
done
if [ "$ok_groups" -eq 0 ]; then
  log "✗ ab_configs 结束：0 个组合成功（网关未起/ssh 不通/docker 拉不到？）→ 非零退出，避免把空结果当成结论"
  exit 2
fi
log "ab_configs 完成：$ok_groups 个组合成功"
