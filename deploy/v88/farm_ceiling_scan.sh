#!/usr/bin/env bash
# ============================================================
# farm_ceiling_scan.sh —— 在**你们的**机器上把 mock 农场的真实上限测出来。
#
# 为什么需要它：客户/实验室的归一值（农场 34–38 核/千请求/秒 @batch=1）是在**128 核**
# 的实验室机上测的；你们现场 mock 机只有 **32 核**、链路是 **10 GbE**。按那个值推算
# 单台 32 核最多 ~800–880 req/s（batch=1），但那是**推算**——先实测拿到真值，
# 再决定"加几台 mock 机器"，而不是靠推算去买机器。
#
# ## 它怎么判"到顶了"（**不依赖 ssh 采远端 CPU**）
#
# 农场饱和的签名与网关过载**不同**：
#   农场到顶 → qps 不再增长、dur 变长、**fail 仍然是 0**（网关只是在排队，从不拒连）
#   网关/链路到顶 → fail 上升、或 RST
# 所以这里只看 k6 侧：**VU 加了但 qps 涨不动（<QPS_GROWTH_MIN）连续两档** ⇒ 判到顶。
#
# ⚠️ 本脚本**故意不**去 ssh 采 mock 机的 CPU：本仓 §13 已按评审意见删掉过一条
# 「远端 coproc 锁 + 远端逐参转发 + 跨机哈希校验」的支路，理由是四段高风险代码换
# 一条已知结论。要农场 CPU 就在 mock 机上自己跑一行采样（README 里给了）。
#
# ## 用法
#
#   deploy/v88/farm_ceiling_scan.sh --env v88.env                 # 默认 group B
#   deploy/v88/farm_ceiling_scan.sh --env v88.env --combo D
#   deploy/v88/farm_ceiling_scan.sh --env v88.env --levels 1000,2000,4000,8000,16000
#
# 产能扫描的固定口径（与对标**必须分开跑**，见 docs/manual/v88-rig.md §4）：
#   KEEP_BODY=false（不解析 body，k6 不再自锁）+ 短稳态（默认 60s）
# ⇒ 结果**不可**当对标数据，只能用来定位农场上限与决定扩多少台。
#
# 退出码：0 = 扫描完成（含判到顶）；2 = 前置失败；3 = 农场配置未生效
# ============================================================
set -uo pipefail

V88_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib_v88.sh
. "$V88_DIR/lib_v88.sh"

ENV_FILE="$V88_DIR/v88.env"
SCAN_COMBO="B"
LEVELS="1000,2000,4000,8000,16000,31000"
SCAN_DURATION="60s"
QPS_GROWTH_MIN="5" # 加了一档 VU，qps 涨幅低于这个百分比就算"涨不动"

while [ $# -gt 0 ]; do
  case "$1" in
    --env) ENV_FILE="$2"; shift 2 ;;
    --combo) SCAN_COMBO="$2"; shift 2 ;;
    --levels) LEVELS="$2"; shift 2 ;;
    --duration) SCAN_DURATION="$2"; shift 2 ;;
    -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) v88_die "未知参数 $1（--help）" ;;
  esac
done

[ -f "$ENV_FILE" ] || v88_die "找不到 $ENV_FILE"
# shellcheck disable=SC1090
. "$ENV_FILE"

TOKEN=${TOKEN:-}
TARGET=${TARGET:-}
[ -n "$TARGET" ] || v88_die "v88.env 里必须写 TARGET"
[ -n "$TOKEN" ] || v88_die "v88.env 里必须写 TOKEN"

MODEL=${MODEL:-mock-model}
MOCKS=${MOCKS:-8081-8104}
MOCK_BATCH=${MOCK_BATCH:-1}
MOCK_TTFT=${MOCK_TTFT:-500}
MOCK_INTERVAL=${MOCK_INTERVAL:-10}
K6_IMAGE=${K6_IMAGE:-grafana/k6:latest}
K6_MEM=${K6_MEM:-8g}
FARM_CORES=${FARM_CORES:-32}
WARMUP=${WARMUP:-20}
GRACEFUL_STOP=${GRACEFUL_STOP:-30s}
STREAM_TIMEOUT=${STREAM_TIMEOUT:-600000}
OUT=${OUT:-$V88_DIR/results/ceiling-$(date -u +%Y%m%dT%H%M%SZ)}

# 产能扫描分界线：KEEP_BODY=false（不解析 body ⇒ k6 不自锁、也不出 chunk/gw_oh 指标）
SCAN_KEEP_BODY="false"

COMBOS_TABLE='B|500|10
D|6000|10
E|250|20
F|3000|20
A|100|50
C|1200|50'
row=$(printf '%s\n' "$COMBOS_TABLE" | awk -F'|' -v c="$SCAN_COMBO" '$1==c{print $2"|"$3}')
[ -n "$row" ] || v88_die "未知组 $SCAN_COMBO"
SCAN_OUT=${row%%|*}
SCAN_TI=${row##*|}

on_exit() {
  local rc=$?
  v88_farm_restore || true
  exit "$rc"
}
trap on_exit EXIT

v88_lint_expansions
v88_check_bundle

FARM_HOSTS=$(v88_farm_hosts "$MOCKS")
FARM_N=$(printf '%s\n' "$FARM_HOSTS" | grep -c .)
[ "$FARM_N" -gt 0 ] || v88_die "MOCKS=$MOCKS 展开后一个口都没有"

mkdir -p "$OUT"
LEDGER="$OUT/env.txt"
SCAN_SHA=$(v88_sha256 "$V88_DIR/k6.js")
{
  printf 'SCAN=ceiling\nGENERATED_AT=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'TARGET=%s\nMODEL=%s\nMOCKS=%s\nMOCK_FARM_PORTS=%s\n' "$TARGET" "$MODEL" "$MOCKS" "$FARM_N"
  printf 'FARM_CORES=%s\nMOCK_BATCH=%s\nSCAN_COMBO=%s\nSCAN_OUT=%s\nSCAN_TI=%s\n' \
    "$FARM_CORES" "$MOCK_BATCH" "$SCAN_COMBO" "$SCAN_OUT" "$SCAN_TI"
  printf 'SCAN_DURATION=%s\nSCAN_KEEP_BODY=%s\nWARMUP=%s\nQPS_GROWTH_MIN=%s\n' \
    "$SCAN_DURATION" "$SCAN_KEEP_BODY" "$WARMUP" "$QPS_GROWTH_MIN"
  printf 'K6_IMAGE=%s\nK6_SCRIPT_SHA256=%s\n' "$K6_IMAGE" "$SCAN_SHA"
  printf 'K6_IMAGE_ID=%s\n' "$(docker image inspect "$K6_IMAGE" --format '{{.Id}}' 2>/dev/null || echo unknown)"
} >"$LEDGER"
PROGRESS="$OUT/progress.log"

v88_log "上限扫描：组 ${SCAN_COMBO}（out=$SCAN_OUT interval=${SCAN_TI}ms） 农场 ${MOCKS}（$FARM_N 口） batch=$MOCK_BATCH"
v88_log "产能口径：KEEP_BODY=$SCAN_KEEP_BODY 稳态=${SCAN_DURATION}（**不是**对标口径，结论只用于定农场规模）"
v88_print_farm_budget "$MOCK_BATCH" "$FARM_CORES" "$SCAN_OUT" "$SCAN_TI"

v88_farm_configure "$FARM_HOSTS" "$MOCK_TTFT" "$SCAN_TI" "$SCAN_OUT" "$MOCK_BATCH" "$LEDGER" || exit 3

lab_lock "v88 ceiling_scan $(hostname)-$$" || v88_die "台架互斥锁被占（${LAB_LOCK}）"

num_of() { # 从 k6 汇总行取数，允许科学计数法/空
  awk -v v="$1" 'BEGIN{ if (v ~ /^-?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?$/) print v }'
}

TABLE="$OUT/ceiling.tsv"
printf 'vus\tqps\tdur_p50_s\tttft_p50_ms\tfail_pct\tactive_streams\trc\n' >"$TABLE"

prev_qps=""
prev_vus=""
knee=0
knee_vus=""
knee_qps=""

for vus in $(printf '%s' "$LEVELS" | tr ',' ' '); do
  log="$OUT/${SCAN_COMBO}_${vus}.log"
  v88_log "档位 VUS=$vus 开始"
  docker rm -f "v88_scan_k6_$$" >/dev/null 2>&1
  docker run --rm --name "v88_scan_k6_$$" -m "$K6_MEM" --network=host \
    -v "$V88_DIR:/k6:ro" \
    -e "TARGET_URL=$TARGET" -e "TOKEN=$TOKEN" -e "MODEL=$MODEL" \
    -e STREAM=true -e IN_TOKENS=100 -e "OUT_TOKENS=$SCAN_OUT" \
    -e "VUS=$vus" -e "DURATION=$SCAN_DURATION" -e "MAX_VUS=$vus" \
    -e "WARMUP=$WARMUP" -e "STREAM_TIMEOUT=$STREAM_TIMEOUT" \
    -e "KEEP_BODY=$SCAN_KEEP_BODY" -e "KEEP_SAMPLE=0" -e "CHUNK_MAX=0" \
    -e "GRACEFUL_STOP=$GRACEFUL_STOP" \
    "$K6_IMAGE" run --compatibility-mode=base \
    --summary-trend-stats='p(50),p(90),p(95),p(99),avg,min,max' \
    /k6/k6.js >"$log" 2>&1 9>&- &
  pid=$!
  wait "$pid"
  rc=$?

  qps=$(num_of "$(tail -50 "$log" | grep 'http_reqs' | grep -oE '[0-9.]+/s' | head -1 | sed 's|/s||')")
  dur=$(num_of "$(tail -50 "$log" | grep 'http_req_duration' | grep -oE 'p\(50\)=[0-9.]+s' | head -1 | sed 's/p(50)=//;s/s$//')")
  ttft=$(num_of "$(tail -50 "$log" | grep 'ttft_ms' | grep -oE 'p\(50\)=[0-9.]+' | head -1 | sed 's/p(50)=//')")
  fail=$(num_of "$(tail -50 "$log" | grep 'http_req_failed' | grep -oE '[0-9.]+%' | head -1 | tr -d '%')")
  # 闭式活跃流：VUS × dur/iter（拿不到 iter 时留空，**不要**用 qps×dur 顶替）
  iter=$(num_of "$(tail -50 "$log" | grep 'iteration_duration' | grep -oE 'p\(50\)=[0-9.]+s' | head -1 | sed 's/p(50)=//;s/s$//')")
  active=""
  if [ -n "$dur" ] && [ -n "$iter" ]; then
    active=$(awk -v v="$vus" -v d="$dur" -v i="$iter" 'BEGIN{ if (i+0>0) printf "%.0f", v*d/i }')
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$vus" "${qps:--}" "${dur:--}" "${ttft:--}" "${fail:--}" "${active:--}" "$rc" >>"$TABLE"
  printf '[%s] VUS=%s qps=%s dur_p50=%ss ttft_p50=%sms fail=%s%% active=%s rc=%s\n' \
    "$(date +%H:%M:%S)" "$vus" "${qps:--}" "${dur:--}" "${ttft:--}" "${fail:--}" "${active:--}" "$rc" | tee -a "$PROGRESS"

  if [ "$rc" != "0" ]; then
    v88_warn "k6 rc=$rc ⇒ 台架已经撑不住这一档（这本身就是一条上限证据）"
    knee=1
    knee_vus=$((vus - 1))
    [ -n "$prev_qps" ] && knee_qps="$prev_qps"
    break
  fi

  # 到顶判定：VU 涨了但 qps 涨不动（连续两档才算，避免单档抖动误判）
  if [ -n "$prev_qps" ] && [ -n "$qps" ]; then
    growth=$(awk -v a="$prev_qps" -v b="$qps" 'BEGIN{ if (a+0>0) printf "%.2f", 100*(b-a)/a; else print "" }')
    if [ -n "$growth" ] && awk -v g="$growth" -v m="$QPS_GROWTH_MIN" 'BEGIN{exit !(g+0 < m+0)}'; then
      if [ "$knee" = "1" ]; then
        knee_vus="$prev_vus"
        knee_qps="$prev_qps"
        v88_log "连续两档 qps 涨幅 <${QPS_GROWTH_MIN}% ⇒ **农场到顶**：≈VUS=$knee_vus / qps=$knee_qps"
        break
      fi
      knee=1
      v88_log "本档 qps 涨幅仅 ${growth}%（<${QPS_GROWTH_MIN}%）→ 再看一档确认"
    else
      knee=0
    fi
  fi
  prev_qps="$qps"
  prev_vus="$vus"
done

echo
echo "==== 上限扫描结果（组 ${SCAN_COMBO}，batch=${MOCK_BATCH}，$FARM_N 口 mock，$FARM_CORES 核）===="
column -t -s $'\t' "$TABLE" 2>/dev/null || cat "$TABLE"
echo
if [ -n "$knee_vus" ]; then
  cores_per_kqps=$(awk -v c="$FARM_CORES" -v q="$knee_qps" 'BEGIN{ if (q+0>0) printf "%.1f", c*1000/q; else print "n/a" }')
  echo "⇒ 农场上限（判据：连续两档 qps 涨幅 <${QPS_GROWTH_MIN}%）：**≈${knee_vus} VU / ${knee_qps} req/s**"
  echo "⇒ 本机实测农场成本：**${cores_per_kqps} 核 / 千请求/秒**（batch=${MOCK_BATCH}，$FARM_N 口 mock）"
  echo "   对照仓库实测：batch=1 为 34–38、batch=5 为 12.6–14.1、batch=10 为 7.1–10.8"
  {
    printf 'CEILING_VUS=%s\nCEILING_QPS=%s\n' "$knee_vus" "$knee_qps"
    printf 'FARM_CORES_PER_KQPS_MEASURED=%s\n' "$cores_per_kqps"
  } >>"$LEDGER"
else
  echo "⇒ 本次**没测到**上限（所有档位都还在涨，或首档就失败）—— 把 LEVELS 往上加再跑一轮"
fi
echo
echo "⇒ 要冲更高档位：按实测的「核 / 千请求/秒」× 目标 req/s ÷ $FARM_CORES 算出**需要几台 mock 机**，"
echo "   横向加机器（deploy/README.md §9）而不是改 batch —— 改 batch 只把每台的成本压低，"
echo "   但那是**台架口径**（首字 +（batch−1)×21ms），报告里会被标出来。"
echo "产出：$OUT"
