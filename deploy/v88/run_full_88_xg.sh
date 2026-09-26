#!/usr/bin/env bash
# ============================================================
# run_full_88_xg.sh —— 客户 `run_full_88.sh` 的 **xg 版**（deploy/v88 框架的驱动器）
#
# 与客户原版的差异（**全部不改 ttft / dur / fail / qps 的口径**）：
#   1. preflight：lint + 封条 + k6 镜像在本机存在 + 农场可达 + 路数一致
#      （客户原版只认挂载路径 ⇒ §1.1 那次「同名不同内容」事故）
#   2. MOCK_BATCH **参数化** + 下发 + **回读校验** + 收尾还原
#      （客户原版第 68 行写死 `batch=1`；手工改过的 batch 会被它静默改回）
#   3. mock 路数**显式**且与 `MOCKS` 展开数一致
#      （客户包自相矛盾：`deploy88/start-mocks.sh` 起 12 口，driver 却按 24 口配）
#   4. **k6 内存护栏 + 崩溃可续**：`--summary-export` 落盘、每档独立；k6 非零退出时
#      自动降采样重试一次**并把它记成口径变更**；容器没起来（镜像缺失等）给真实原因，不装 OOM
#   5. 每档写 ledger（生效 batch / 路数 / k6 镜像 ID / 脚本 sha256 / 采样参数）
#
# 用法（**不读任何配置文件**：口径全部走环境变量，未给的用保守默认）：
#   TARGET=http://<入口>/v1/chat/completions TOKEN=<key> deploy/v88/run_full_88_xg.sh
#   deploy/v88/run_full_88_xg.sh --dry-run          # 只打印计划，不碰农场
#   deploy/v88/run_full_88_xg.sh --preflight-only   # 只跑前置检查
#   deploy/v88/run_full_88_xg.sh                    # 真跑
#   deploy/v88/run_full_88_xg.sh --combo B --levels 1000,4000
#
# 退出码：0 = 跑完；2 = 前置检查/配置失败；3 = 农场配置未生效（中止）
# ============================================================
set -uo pipefail

V88_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/lib_v88.sh
. "$V88_DIR/lib/lib_v88.sh"

DRY_RUN=0
PREFLIGHT_ONLY=0
ONLY_COMBO=""
ONLY_LEVELS=""

usage() {
  sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --preflight-only) PREFLIGHT_ONLY=1; shift ;;
    --combo) ONLY_COMBO="$2"; shift 2 ;;
    --levels) ONLY_LEVELS="$2"; shift 2 ;;
    -h|--help) usage ;;
    *) v88_die "未知参数 $1（--help）" ;;
  esac
done

# ---- 默认值：口径全部来自环境变量，未给的用保守默认；**不读任何文件** ----
MODEL=${MODEL:-mock-model}
MOCKS=${MOCKS:-8081-8104}
MOCK_BATCH=${MOCK_BATCH:-1}
MOCK_TTFT=${MOCK_TTFT:-500}
MOCK_INTERVAL=${MOCK_INTERVAL:-10}
K6_IMAGE=${K6_IMAGE:-grafana/k6:latest}
K6_MEM=${K6_MEM:-45g}
KEEP_BODY=${KEEP_BODY:-true}
KEEP_SAMPLE=${KEEP_SAMPLE:-1}
CHUNK_MAX=${CHUNK_MAX:-64}
GRACEFUL_STOP=${GRACEFUL_STOP:-600s}
STREAM_TIMEOUT=${STREAM_TIMEOUT:-600000}
START_VUS=${START_VUS:-1000}
STEP=${STEP:-1000}
MAX_VUS=${MAX_VUS:-60000}
WARMUP=${WARMUP:-30}
STABLE_SHORT=${STABLE_SHORT:-300s}
STABLE_LONG=${STABLE_LONG:-640s}
COMBOS=${COMBOS:-"B D E F A C"}
RESULT_DIR=${RESULT_DIR:-$V88_DIR/results/$(date -u +%Y%m%dT%H%M%SZ)}
FARM_MEM_BEFORE=""

# 客户原版的分组表（out|token_interval|稳态）。**不要改**：它是对标口径的一部分。
COMBOS_TABLE='B|500|10|SHORT
D|6000|10|LONG
E|250|20|SHORT
F|3000|20|LONG
A|100|50|SHORT
C|1200|50|LONG'

combo_row() { # <组> → out|interval|long/short
  printf '%s\n' "$COMBOS_TABLE" | awk -F'|' -v c="$1" '$1==c{print $2"|"$3"|"$4}'
}

TARGET=${TARGET:-}
TOKEN=${TOKEN:-}
[ -n "$TARGET" ] || v88_die "必须给 TARGET（环境变量），例：TARGET=http://<入口>/v1/chat/completions TOKEN=<key> $0"
[ -n "$TOKEN" ] || v88_die "必须给 TOKEN（环境变量），与 TARGET 一起传"

# 档间排空（客户 `run_full_88.sh` 的 wait_drain 口径）：GW_HOST 不给就从 TARGET 里取
GW_HOST=${GW_HOST:-$(printf '%s' "$TARGET" | sed -E 's#^[a-zA-Z][a-zA-Z0-9+.-]*://##; s#/.*$##; s#:[0-9]+$##')}
BASELINE_CONN=${BASELINE_CONN:-300}
MAX_WAIT_SEC=${MAX_WAIT_SEC:-300}

# ---------------------------------------------------------------- 退出清理
# 只挂 EXIT：实测 bash 在 `wait` 期间收到 TERM/INT 会执行 EXIT trap（额外的
# `trap 'exit 130' INT` 不必要，见 docs/changelog.md 那条台架修复）。
# ⚠️ `kill -9` 下**不执行** ⇒ 农场会停在改过的 batch 上。所以每轮都回读记账。
on_exit() {
  local rc=$?
  v88_farm_restore || true
  exit "$rc"
}
trap on_exit EXIT

# ---------------------------------------------------------------- 前置检查
v88_lint_expansions
v88_check_bundle

# k6 镜像必须**本机已在**：现场驱动机多半拉不动 docker.io（本轮实测：DNS 对
# registry-1.docker.io 直接 refused），而 `docker run` 对缺失镜像是 rc=125 秒挂，
# 之前被台架误报成「OOM」排查了半天。fail-closed，三选一给出路。
if ! docker image inspect "$K6_IMAGE" >/dev/null 2>&1; then
  v88_die "k6 镜像 ${K6_IMAGE} 本机不存在，而 docker run 会直接 rc=125（现场拉 docker.io 大概率不通）
   ⇒ 三选一：
     docker tag <镜像源全名> ${K6_IMAGE}      # 例：docker tag docker.m.daocloud.io/grafana/k6:latest ${K6_IMAGE}
     K6_IMAGE=<本机已有的 k6 镜像全名> 重跑（ledger 会记实际镜像）
     docker load -i <k6.tar> 后重跑"
fi

FARM_HOSTS=$(v88_farm_hosts "$MOCKS")
FARM_N=$(printf '%s\n' "$FARM_HOSTS" | grep -c .)
[ "$FARM_N" -gt 0 ] || v88_die "MOCKS=$MOCKS 展开后一个口都没有"

v88_log "目标 $TARGET  模型 $MODEL"
v88_log "农场 ${MOCKS}（$FARM_N 口）  batch=$MOCK_BATCH  ttft=$MOCK_TTFT  interval=$MOCK_INTERVAL"

# 农场可达性 + 口径回读（客户原版完全没有这层）
unreachable=0
for h in $FARM_HOSTS; do
  b=$(v88_farm_read_batch "$h")
  if [ -z "$b" ]; then
    v88_warn ":$h 的 /_config 读不到 batch —— 不是本仓 Node 农场、也不是 Go mock？"
    unreachable=$((unreachable + 1))
  fi
done
[ "$unreachable" -eq 0 ] || v88_die "有 $unreachable/$FARM_N 口读不到口径，先修农场再压（否则静默失效会产出假结论）"

K6_SHA=$(v88_sha256 "$V88_DIR/k6.js")
v88_log "k6.js sha256=${K6_SHA}  KEEP_BODY=$KEEP_BODY KEEP_SAMPLE=$KEEP_SAMPLE CHUNK_MAX=$CHUNK_MAX"
if [ "$PREFLIGHT_ONLY" = "1" ]; then
  v88_log "preflight-only：以上全部通过，未开跑"
  trap - EXIT
  exit 0
fi

# ---------------------------------------------------------------- 结果目录与 ledger
mkdir -p "$RESULT_DIR"
LEDGER="$RESULT_DIR/env.txt"
{
  printf 'GENERATED_AT=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'TARGET=%s\nMODEL=%s\nMOCKS=%s\nMOCK_FARM_PORTS=%s\n' "$TARGET" "$MODEL" "$MOCKS" "$FARM_N"
  printf 'KEEP_BODY=%s\nKEEP_SAMPLE=%s\nCHUNK_MAX=%s\n' "$KEEP_BODY" "$KEEP_SAMPLE" "$CHUNK_MAX"
  printf 'GRACEFUL_STOP=%s\nSTREAM_TIMEOUT=%s\n' "$GRACEFUL_STOP" "$STREAM_TIMEOUT"
  printf 'GW_HOST=%s\nBASELINE_CONN=%s\nMAX_WAIT_SEC=%s\n' "$GW_HOST" "$BASELINE_CONN" "$MAX_WAIT_SEC"
  printf 'K6_IMAGE=%s\nK6_SCRIPT_SHA256=%s\n' "$K6_IMAGE" "$K6_SHA"
  printf 'K6_IMAGE_ID=%s\n' "$(docker image inspect "$K6_IMAGE" --format '{{.Id}}' 2>/dev/null || echo unknown)"
  printf 'COMBOS=%s\nSTART_VUS=%s\nSTEP=%s\nMAX_VUS=%s\n' "$COMBOS" "$START_VUS" "$STEP" "$MAX_VUS"
} >"$LEDGER"
PROGRESS="$RESULT_DIR/progress.log"
v88_log "结果目录 $RESULT_DIR"

lab_lock "v88 run_full_88_xg $(hostname)-$$" || v88_die "台架互斥锁被占（见 ${LAB_LOCK}）"

# ---------------------------------------------------------------- 档间排空
# 客户 `run_full_88.sh` 同形：每档开跑前等网关 ESTAB 回落到基线。
# `ss` 缺失（macOS 开发机）⇒ 计数为 0 ⇒ 立即返回，与客户脚本行为一致。
wait_drain() {
  local waited=0 conns
  printf '[%s] 等待残余连接清空（基线=%s，最多 %ss）…\n' \
    "$(date +%H:%M:%S)" "$BASELINE_CONN" "$MAX_WAIT_SEC" | tee -a "$PROGRESS"
  while [ "$waited" -lt "$MAX_WAIT_SEC" ]; do
    conns=$(ss -tan 2>/dev/null | grep ESTAB | grep -c "$GW_HOST" || true)
    conns=${conns:-0}
    printf '[%s]   网关连接数=%s（已等 %ss）\n' "$(date +%H:%M:%S)" "$conns" "$waited" | tee -a "$PROGRESS"
    if [ "$conns" -le "$BASELINE_CONN" ]; then
      v88_log "残余连接已清空 ✓"
      return 0
    fi
    sleep 15
    waited=$((waited + 15))
  done
  v88_warn "超过 ${MAX_WAIT_SEC}s 未清空（连接数=${conns:-?}），继续执行（客户原版同样只告警）"
  return 0
}

# ---------------------------------------------------------------- k6 单档
# 后台跑 + `wait`：前台 `docker run` 期间收到 TERM 时 bash 会把 trap 推迟到它结束，
# 于是"停不下来、锁不释放"（ab_configs 踩过，见 docs/changelog.md）。`9>&-` 保证
# 子进程不继承台架锁的 fd。
run_k6_once() { # <combo> <vus> <stable> <out> <ti> <keep_sample> <chunk_max> → 日志路径
  local combo="$1" vus="$2" stable="$3" out="$4" ti="$5" ks="$6" cm="$7"
  local stem="${combo}_${vus}"
  local log="$RESULT_DIR/$stem.log"
  docker rm -f "v88_k6_$$" >/dev/null 2>&1
  docker run --rm --name "v88_k6_$$" -m "$K6_MEM" --network=host \
    -v "$V88_DIR:/k6:ro" -v "$RESULT_DIR:/out" \
    -e "TARGET_URL=$TARGET" -e "TOKEN=$TOKEN" -e "MODEL=$MODEL" \
    -e STREAM=true -e IN_TOKENS=100 -e "OUT_TOKENS=$out" \
    -e "VUS=$vus" -e "DURATION=$stable" -e "MAX_VUS=$MAX_VUS" \
    -e "WARMUP=$WARMUP" -e "STREAM_TIMEOUT=$STREAM_TIMEOUT" \
    -e "KEEP_BODY=$KEEP_BODY" -e "KEEP_SAMPLE=$ks" \
    -e "CHUNK_MAX=$cm" -e "GRACEFUL_STOP=$GRACEFUL_STOP" \
    "$K6_IMAGE" run --compatibility-mode=base \
    --summary-export="/out/$stem.summary.json" \
    --summary-trend-stats='p(50),p(90),p(95),p(99),avg,min,max' \
    /k6/k6.js >"$log" 2>&1 9>&- &
  local pid=$!
  wait "$pid"
  local rc=$?
  printf '%s' "$rc" >"$RESULT_DIR/$stem.rc"
  printf '%s' "$log"
}

# 「容器根本没起来」的特征行（docker run 自身失败，rc=125）：镜像缺失 / daemon 拒绝。
# 与 OOM（容器被 kill，rc=137，且 k6 已产出部分输出）是两回事 —— 本轮现场把 125
# 当 OOM 排查了一圈，其实日志第一行就是 Unable to find image。
v88_k6_startup_failure() { # <log> → docker 错误行（非空 = 容器没起来）
  grep -m1 -E "Unable to find image|Error response from daemon" "$1" 2>/dev/null || true
}

# 客户原版那套 `tail -50 | grep` 解析（保持同形，便于与历史日志对齐），
# 另加 `chunk_samples_added`（有界采样的有效样本数，必须报出来）。
parse_log() { # <log> <out> → 一行摘要
  local log="$1" out="$2"
  local fail dur50 dur90 dur99 t50 t99 tav chunk50 chunk99 gw50 gw99 qps iter cok csamp
  fail=$(tail -50 "$log" | grep 'http_req_failed' | grep -oE '[0-9.]+%' | head -1 | tr -d '%')
  dur50=$(tail -50 "$log" | grep 'http_req_duration' | grep -oE 'p\(50\)=[0-9a-zµ.]+' | head -1 | sed 's/p(50)=//')
  dur90=$(tail -50 "$log" | grep 'http_req_duration' | grep -oE 'p\(90\)=[0-9a-zµ.]+' | head -1 | sed 's/p(90)=//')
  dur99=$(tail -50 "$log" | grep 'http_req_duration' | grep -oE 'p\(99\)=[0-9a-zµ.]+' | head -1 | sed 's/p(99)=//')
  t50=$(tail -50 "$log" | grep 'ttft_ms' | grep -oE 'p\(50\)=[0-9.]+' | head -1 | sed 's/p(50)=//')
  t99=$(tail -50 "$log" | grep 'ttft_ms' | grep -oE 'p\(99\)=[0-9.]+' | head -1 | sed 's/p(99)=//')
  tav=$(tail -50 "$log" | grep 'ttft_ms' | grep -oE 'avg=[0-9.]+' | head -1 | sed 's/avg=//')
  chunk50=$(tail -50 "$log" | grep 'chunk_interval_ms' | grep -oE 'p\(50\)=[0-9.]+[mµs]*' | head -1 | sed 's/p(50)=//')
  chunk99=$(tail -50 "$log" | grep 'chunk_interval_ms' | grep -oE 'p\(99\)=[0-9.]+[mµs]*' | head -1 | sed 's/p(99)=//')
  gw50=$(tail -50 "$log" | grep 'gateway_overhead_ms' | grep -oE 'p\(50\)=-?[0-9.]+[mµs]*' | head -1 | sed 's/p(50)=//')
  gw99=$(tail -50 "$log" | grep 'gateway_overhead_ms' | grep -oE 'p\(99\)=-?[0-9.]+[mµs]*' | head -1 | sed 's/p(99)=//')
  qps=$(tail -50 "$log" | grep 'http_reqs' | grep -oE '[0-9.]+/s' | head -1 | sed 's|/s||')
  iter=$(tail -50 "$log" | grep -E 'iterations\.+:|iterations +[0-9]' | grep -v duration | head -1 | grep -oE '[0-9]+' | head -1)
  cok=$(tail -50 "$log" | grep 'checks_succeeded' | grep -oE '[0-9.]+%' | head -1)
  csamp=$(tail -50 "$log" | grep 'chunk_samples_added' | grep -oE '[0-9]+' | head -1)
  local tps
  tps=$(printf '%s' "$qps" | awk -v o="$out" '{if($1!="") printf "%.0f", $1*o}')
  printf 'fail=%s%% checks_ok=%s | dur p50=%s p90=%s p99=%s | ttft p50=%s p99=%s avg=%s | chunk_int p50=%s p99=%s (samples=%s) | gw_oh p50=%s p99=%s | qps=%s tps=%s | iter=%s' \
    "${fail:-?}" "${cok:-?}" "${dur50:-?}" "${dur90:-?}" "${dur99:-?}" "${t50:-?}" "${t99:-?}" "${tav:-?}" \
    "${chunk50:-?}" "${chunk99:-?}" "${csamp:-?}" "${gw50:-?}" "${gw99:-?}" "${qps:-?}" "${tps:-?}" "${iter:-?}"
}

# ---------------------------------------------------------------- 主循环
run_group() { # <combo> <out> <ti> <stable>
  local combo="$1" out="$2" ti="$3" stable="$4"
  local row vus
  v88_log ">>>>>> 组 $combo 开始（out=$out interval=${ti}ms 稳态=${stable}）"
  printf '[%s] >>>>>> 组 %s (out=%s ti=%s 稳态=%s) batch=%s\n' \
    "$(date +%H:%M:%S)" "$combo" "$out" "$ti" "$stable" "$MOCK_BATCH" >>"$PROGRESS"

  # 每组分发一次（客户原版同形）——参数化 batch + 回读校验 + 记账
  v88_farm_configure "$FARM_HOSTS" "$MOCK_TTFT" "$ti" "$out" "$MOCK_BATCH" "$LEDGER" || return 3
  sleep 2   # 客户原版同形

  # 客户原版同形的每组冒烟：经网关打一发并数 `data:` 行（只打印，不中止）
  local sm_log="$RESULT_DIR/${combo}_smoke.log" sm_body="$RESULT_DIR/${combo}_smoke.body" smoke_chunks
  curl -s -N -m 120 -X POST "$TARGET" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"stream\":true,\"stream_options\":{\"include_usage\":true}}" \
    -o "$sm_body" -w 'http=%{http_code} t=%{time_total}s\n' >"$sm_log" 2>&1 || true
  smoke_chunks=$(grep -c '^data:' "$sm_body" 2>/dev/null || true)
  printf '  冒烟: %s chunks=%s\n' "$(cat "$sm_log" 2>/dev/null)" "${smoke_chunks:-0}" | tee -a "$PROGRESS"
  [ "${smoke_chunks:-0}" -gt 0 ] || v88_warn "冒烟拿到 0 个 data: 行（客户原版只打印，这里同样继续）"

  vus=$START_VUS
  local failed_prev=0
  while :; do
    if [ -n "$ONLY_LEVELS" ] && ! printf ',%s,' "$ONLY_LEVELS" | grep -q ",$vus,"; then
      vus=$((vus + STEP))
      [ "$vus" -le "$MAX_VUS" ] || break
      continue
    fi

    wait_drain   # 客户原版同形：每档开跑前排空残余连接
    v88_log "组 $combo VUS=$vus 开始"
    local log rc
    log=$(run_k6_once "$combo" "$vus" "$stable" "$out" "$ti" "$KEEP_SAMPLE" "$CHUNK_MAX")
    rc=$(cat "$RESULT_DIR/${combo}_${vus}.rc" 2>/dev/null || echo 1)

    if [ "$rc" != "0" ]; then
      # 先分清两种失败：容器**没起来**（docker rc=125：镜像缺失/daemon 拒绝）与
      # 真跑起来后被 kill（OOM ⇒ rc=137）。前者降采样救不了，直说原因并 INVALID；
      # 后者才是下面的降采样重试路径。
      local startfail
      startfail=$(v88_k6_startup_failure "$log")
      if [ -n "$startfail" ]; then
        v88_warn "k6 rc=${rc}：容器没起来（非 OOM）—— ${startfail}"
        v88_warn "完整日志在 ${log}；修好镜像/daemon 后重跑本档"
        printf '[%s] ✗ %s VUS=%s k6 容器未启动：%s ⇒ INVALID\n' \
          "$(date +%H:%M:%S)" "$combo" "$vus" "$startfail" >>"$PROGRESS"
        break
      fi
      # k6 崩溃不是"这一轮废了"，而是**台架侧的信号**。降采样重试一次，
      # 并把降级**记为口径变更**（checks 比会从 2.00 变 1.10），不静默。
      v88_warn "k6 退出码 ${rc}（疑似 OOM/被 kill）⇒ 降采样重试一次：KEEP_SAMPLE=0.1 CHUNK_MAX=16"
      printf '[%s] ⚠️ %s VUS=%s k6 rc=%s → 降采样重试（口径变更，checks 比将 <2.00）\n' \
        "$(date +%H:%M:%S)" "$combo" "$vus" "$rc" >>"$PROGRESS"
      log=$(run_k6_once "$combo" "$vus" "$stable" "$out" "$ti" "0.1" "16")
      rc=$(cat "$RESULT_DIR/${combo}_${vus}.rc" 2>/dev/null || echo 1)
      printf 'DOWNGRADED_LEVELS=%s_%s\n' "$combo" "$vus" >>"$LEDGER"
      if [ "$rc" != "0" ]; then
        v88_warn "降采样后 k6 仍然 rc=$rc ⇒ 这一档记为 INVALID 并跳到下一组"
        printf '[%s] ✗ %s VUS=%s k6 两次失败 rc=%s ⇒ INVALID\n' "$(date +%H:%M:%S)" "$combo" "$vus" "$rc" >>"$PROGRESS"
        break
      fi
    fi

    local summary
    summary=$(parse_log "$log" "$out")
    printf '[%s] %s VUS=%s %s\n' "$(date +%H:%M:%S)" "$combo" "$vus" "$summary" | tee -a "$PROGRESS"

    # 客户原版的停测规则：连续两档 fail > 1%
    local fail over
    fail=$(tail -50 "$log" | grep 'http_req_failed' | grep -oE '[0-9.]+%' | head -1 | tr -d '%')
    over=0
    [ -n "$fail" ] && over=$(awk -v f="$fail" 'BEGIN{print (f>1)?1:0}')
    if [ "$over" = "1" ]; then
      if [ "$failed_prev" = "1" ]; then
        v88_log "组 $combo 连续两档 >1%（VUS=$vus fail=${fail}%）→ STOP"
        break
      fi
      failed_prev=1
    else
      failed_prev=0
    fi

    vus=$((vus + STEP))
    if [ "$vus" -gt "$MAX_VUS" ]; then
      v88_log "组 $combo 达到上限 $MAX_VUS → STOP"
      break
    fi
  done
  v88_log ">>>>>> 组 $combo 完成"
}

printf '[%s] ====== V88 xg 版开始 | 组 %s | batch=%s | 农场 %s 口 | %sVU 起 +%s | 上限 %s ======\n' \
  "$(date +%H:%M:%S)" "$COMBOS" "$MOCK_BATCH" "$FARM_N" "$START_VUS" "$STEP" "$MAX_VUS" | tee -a "$PROGRESS"
printf '口径：KEEP_BODY=%s KEEP_SAMPLE=%s CHUNK_MAX=%s gracefulStop=%s | k6.js sha256=%s\n' \
  "$KEEP_BODY" "$KEEP_SAMPLE" "$CHUNK_MAX" "$GRACEFUL_STOP" "$K6_SHA" | tee -a "$PROGRESS"

if [ "$DRY_RUN" = "1" ]; then
  v88_log "dry-run：计划如上；**未下发农场、未跑 k6**"
  for c in $COMBOS; do
    [ -z "$ONLY_COMBO" ] || [ "$c" = "$ONLY_COMBO" ] || continue
    row=$(combo_row "$c")
    [ -n "$row" ] || v88_die "未知组 $c"
    printf '  组 %s: out=%s interval=%sms\n' "$c" \
      "${row%%|*}" "$(printf '%s' "$row" | cut -d'|' -f2)"
  done
  trap - EXIT
  exit 0
fi

rc_all=0
for c in $COMBOS; do
  [ -z "$ONLY_COMBO" ] || [ "$c" = "$ONLY_COMBO" ] || continue
  row=$(combo_row "$c")
  [ -n "$row" ] || v88_die "未知组 ${c}（COMBOS_TABLE 里没有）"
  out=${row%%|*}
  rest=${row#*|}
  ti=${rest%%|*}
  len=${rest##*|}
  stable=$STABLE_SHORT
  [ "$len" = "LONG" ] && stable=$STABLE_LONG
  run_group "$c" "$out" "$ti" "$stable" || rc_all=3
done

v88_log "====== V88 xg 版完成 | 结果目录 $RESULT_DIR ======"
exit "$rc_all"
