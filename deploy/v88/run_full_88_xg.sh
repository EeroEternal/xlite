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
#   6. **停测规则默认关闭**（`STOP_ON_FAIL=0`）：跑到 `MAX_VUS`，不在 fail>1% 处截断 ——
#      过载段那几档的 fail 本身就是结论，提前停掉只会留下缺口、事后无法解释。
#      设 `STOP_ON_FAIL=1` 恢复客户原版「连续两档 fail>1% → STOP」；**指标定义不变**。
#   7. **失败分类**：k6 侧把「传输错误 / 502 / 503 / 其它非 200」拆成独立 Counter，每档摘要
#      多打 `split(t= 502= 503= other=)`、`reqs=`（识别「这档请求数异常」）与 `rst=`/`dial=`。
#   8. **阶梯形状写进 env.txt，且上界不得低于 v88-dual**：`MAX_VUS` 低于对方就少行。
#      要缩墙钟只能调 `STEP_COARSE_ABOVE`/`STEP_COARSE`、稳态，或用 `RESUME=1` 分轮跑。
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
  sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
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
K6_MEM=${K6_MEM:-80g}
KEEP_BODY=${KEEP_BODY:-true}
KEEP_SAMPLE=${KEEP_SAMPLE:-}
CHUNK_MAX=${CHUNK_MAX:-64}
GRACEFUL_STOP=${GRACEFUL_STOP:-900s}
STREAM_TIMEOUT=${STREAM_TIMEOUT:-600000}
# 停测规则。客户原版是「连续两档 fail>1% → STOP」；本 driver 默认 **不停**：
# 过载段那几档的 fail 本身就是要看的结论，提前截断只能得到一段无法解释的缺口。
# 判定仍用 k6 内置 `http_req_failed`（与客户同口径），不改任何指标定义。
STOP_ON_FAIL=${STOP_ON_FAIL:-0}
START_VUS=${START_VUS:-1000}
STEP=${STEP:-1000}
MAX_VUS=${MAX_VUS:-60000}
WARMUP=${WARMUP:-30}
STABLE_SHORT=${STABLE_SHORT:-300s}
STABLE_LONG=${STABLE_LONG:-640s}
# 收窄阶梯**只能用步进/稳态**，不许降上界：v88-dual（客户双节点那轮）跑到 60000，
# 我们低一档，对标表就直接缺行。默认本就是 60000，这里只是把约束写成代码。
V88_DUAL_MAX_VUS=${V88_DUAL_MAX_VUS:-60000}
# 两段步进（**默认开**）：低载段保持 1000 的分辨率（对标最干净的那张表——1000~4000 VU 的
# TTFT 就在这一段），VU 到 STEP_COARSE_ABOVE 之后放粗到 STEP_COARSE。**不是**客户那种
# 逐档 1000 的阶梯，所以会写进 env.txt 并在 progress 打 ⚠️。
# 想回到与客户完全一致的密阶梯：`STEP_COARSE_ABOVE=0`。
STEP_COARSE_ABOVE=${STEP_COARSE_ABOVE:-10000}
STEP_COARSE=${STEP_COARSE:-5000}
# 续跑：`RESUME=1` 且 `RESULT_DIR` 指向旧轮次时，已有 `rc=0` 的档直接复用其日志、
# 不再跑 k6。用途是把一场几十小时的阶梯切成几轮（每轮几小时），而不是赌一轮不被打断。
# 复用的档在 progress 里标 `♻`，不会静默冒充新数据。
RESUME=${RESUME:-0}
COMBOS=${COMBOS:-"B D E F A C"}
RESULT_DIR=${RESULT_DIR:-$V88_DIR/results/$(date -u +%Y%m%dT%H%M%SZ)}
FARM_MEM_BEFORE=""

# ---- 阶梯形状：先把口径卡住，再碰农场 ----------------------------------------
if [ "$MAX_VUS" -lt "$V88_DUAL_MAX_VUS" ]; then
  v88_die "MAX_VUS=$MAX_VUS 低于 v88-dual 的 ${V88_DUAL_MAX_VUS}：对标表会缺行。要缩一场跑测请调 STEP_COARSE_ABOVE/STEP_COARSE、STABLE_SHORT/LONG，或用 RESUME=1 分轮，**不要降上界**。"
fi
if [ "$STEP_COARSE_ABOVE" -gt 0 ] && [ "$STEP_COARSE" -le 0 ]; then
  v88_die "STEP_COARSE_ABOVE=$STEP_COARSE_ABOVE 但 STEP_COARSE 未给（需 >0）"
fi
if [ "$START_VUS" -gt "$MAX_VUS" ]; then
  v88_die "START_VUS=$START_VUS 大于 MAX_VUS=${MAX_VUS}：阶梯为空"
fi

# ---- 时长与内存的关系护栏（两个“看不见的洞”）-------------------------------
# `GRACEFUL_STOP`（Go 时长字面量，如 `900s` / `10m0s`）必须比 `STREAM_TIMEOUT`（毫秒）留出余量：
# 否则在载荷窗口末尾发起的请求，会在“被记成失败”之前就被 k6 掐掉 ⇒ `fail%` 与 p99 被**低估**。
# 实测 0927 / 0924 两轮的 `interrupted iterations` 都是 0（这条余量从未被触发），所以它是**防回归**。
# 不 fail-loud 的原因：客户那份口径就是 `grace == timeout`（`600s` / `600000ms`），
# 要 1:1 复刻它就必须允许余量 0 —— 但那样洞是开着的，所以只打出来给人看。
dur_secs() { # <Go 时长字面量> → 秒（支持 900s / 10m / 10m0s / 1h；不支持小数）
  local rest="$1" total=0 part
  [ -n "$rest" ] || return 1
  while [ -n "$rest" ]; do
    part=$(printf '%s' "$rest" | sed -nE 's/^([0-9]+[hms]).*/\1/p')
    [ -n "$part" ] || return 1
    case "$part" in
      *h) total=$((total + ${part%h} * 3600)) ;;
      *m) total=$((total + ${part%m} * 60)) ;;
      *s) total=$((total + ${part%s})) ;;
    esac
    rest=${rest#"$part"}
  done
  printf '%s' "$total"
}

case "$STREAM_TIMEOUT" in
  ''|*[!0-9]*) v88_die "STREAM_TIMEOUT=${STREAM_TIMEOUT} 必须是毫秒整数（如 600000）" ;;
esac
GS_SECS=$(dur_secs "$GRACEFUL_STOP") || v88_die "GRACEFUL_STOP=${GRACEFUL_STOP} 不是可解析的时长（例 900s / 10m0s）"
ST_SECS=$((STREAM_TIMEOUT / 1000))
GRACE_MARGIN_S=$((GS_SECS - ST_SECS))
if [ "$GRACE_MARGIN_S" -lt 60 ]; then
  v88_warn "GRACEFUL_STOP=${GRACEFUL_STOP} 相对 STREAM_TIMEOUT=${STREAM_TIMEOUT}ms 只余 ${GRACE_MARGIN_S}s：窗口末尾发起的请求可能在记成失败前被 k6 掐掉（fail% 与 p99 会被低估）。客户口径就是 0 余量，所以只告警不拦；要留余量请用 GRACEFUL_STOP=$((ST_SECS + 300))s。"
fi

# `K6_MEM` 是 docker 的 `-m`：**上限，不是预留**。它同时是本机 k6 的护盾 ——
# 设小了 k6 自己被杀（rc=137；0927 那轮 5 档如此，被迫降到 0.01 采样重跑）；
# 设大了护盾消失，宿主机 OOM killer 挑受害者，而现场 `.29` 上同时住着 24 个 mock。
# 只记录 + 告警，不 die：农场可能不在本机（`MOCKS` 支持跮机），判断依据不足。
HOST_MEM_TOTAL_G=""
if [ -r /proc/meminfo ]; then
  HOST_MEM_TOTAL_G=$(awk '/^MemTotal:/{printf "%d", $2/1048576}' /proc/meminfo 2>/dev/null || true)
fi
K6_MEM_G=$(printf '%s' "$K6_MEM" | sed -nE 's/^([0-9]+)[gG]?$/\1/p')
if [ -n "$HOST_MEM_TOTAL_G" ] && [ -n "$K6_MEM_G" ] && [ "$((K6_MEM_G * 2))" -gt "$HOST_MEM_TOTAL_G" ]; then
  v88_warn "K6_MEM=${K6_MEM} 超过本机内存 ${HOST_MEM_TOTAL_G}G 的一半：docker 的 -m 是上限不是预留，k6 长到那里时被宿主机 OOM killer 杀掉的可能不是它 —— 本机如果同时跑 mock 农场，风险更高。"
fi

# 某一档之后要加的步进。阶梯的**唯一**步进来源：主循环、ONLY_LEVELS 跳过、计划打印
# 三处都调它，避免三份实现漂移。
step_for() {
  if [ "$STEP_COARSE_ABOVE" -gt 0 ] && [ "$1" -ge "$STEP_COARSE_ABOVE" ]; then
    printf '%s' "$STEP_COARSE"
  else
    printf '%s' "$STEP"
  fi
}

# 与主循环同形的档位枚举（`START_VUS` 起，`> MAX_VUS` 停）。
ladder_levels() {
  local v="$START_VUS" next
  while [ "$v" -le "$MAX_VUS" ]; do
    printf '%s\n' "$v"
    next=$((v + $(step_for "$v")))
    # 末档必须**正好**落在 MAX_VUS：步进一粗，算术就会跳过它
    # （例：START=1000 / STEP=5000 ⇒ …56000 就停了）。上界不能比 v88-dual 低，所以补一档。
    if [ "$next" -gt "$MAX_VUS" ] && [ "$v" -ne "$MAX_VUS" ]; then
      printf '%s\n' "$MAX_VUS"
      return
    fi
    v="$next"
  done
}
NEWCOL_CHECKED=0

# `--levels` 是**阶梯的过滤器**，不是在阶梯之外另开档位：值不在阶梯上会被静默跳过
# （实测：默认阶梯下 `--levels 44000` 一档都不跑；`--levels 11000,31000,55000` 只跑 55000 ——
# 因为默认阶梯是 1000 起 +1000、且 VU ≥ 10000 改 5000 步进，11000/31000 根本不在上面）。
# 这类静默跳过会产出「看起来跑完了一轮、其实只跑了几档」的结果目录 —— 而档位值最容易
# 从**旧结果的文件名**（如 `B_11000.log`）抄来，旧轮次的阶梯与当前未必一样。
# 所以这里 fail-loud，并把当前阶梯列出来供直接复制。
if [ -n "$ONLY_LEVELS" ]; then
  ladder_csv=",$(ladder_levels | tr '\n' ',' | sed 's/,$//'),"
  ladder_show=${ladder_csv#,}; ladder_show=${ladder_show%,}
  levels_bad=""
  for want in $(printf '%s' "$ONLY_LEVELS" | tr ',' ' '); do
    case "$ladder_csv" in
      *",$want,"*) ;;
      *) levels_bad="$levels_bad $want" ;;
    esac
  done
  if [ -n "$levels_bad" ]; then
    v88_die "--levels 里的这些档位不在当前阶梯上：${levels_bad# }
   当前阶梯：${ladder_show}
   取值只能从上面这一串里选。提示：档位值如果是照旧结果的文件名（如 B_11000.log）抄的，
   旧轮次的阶梯与当前未必一样（见 STARTUP.md §5.1）。"
  fi
fi

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

# 客户原版的分组采样率（v88-dual 口径：body 保留比例，防止超长流打爆 k6 内存）：
#   A/E 轻短流=100% | B=50%(100%会在58500撞墙) | C/F=10% | D=5%
combo_sample() { # <组> → 采样率
  if [ -n "${KEEP_SAMPLE:-}" ]; then
    printf '%s' "$KEEP_SAMPLE"
    return
  fi
  case "$1" in
    A|E) printf '1.0' ;;
    B)   printf '0.5' ;;
    C|F) printf '0.1' ;;
    D)   printf '0.05' ;;
    *)   printf '1.0' ;;
  esac
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
v88_log "农场 ${MOCKS}（$FARM_N 口）  ttft=$MOCK_TTFT  interval=$MOCK_INTERVAL"

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
if [ -n "${KEEP_SAMPLE:-}" ]; then
  v88_log "k6.js sha256=${K6_SHA}  KEEP_BODY=${KEEP_BODY} KEEP_SAMPLE=${KEEP_SAMPLE}(全局固定) CHUNK_MAX=${CHUNK_MAX}"
else
  v88_log "k6.js sha256=${K6_SHA}  KEEP_BODY=${KEEP_BODY} 采样=客户分组(A/E=100% B=50% C/F=10% D=5%) CHUNK_MAX=${CHUNK_MAX}"
fi
if [ "$PREFLIGHT_ONLY" = "1" ]; then
  v88_log "preflight-only：以上全部通过，未开跑"
  trap - EXIT
  exit 0
fi

# ---------------------------------------------------------------- 结果目录与 ledger
mkdir -p "$RESULT_DIR"
chmod 777 "$RESULT_DIR"
LEDGER="$RESULT_DIR/env.txt"
{
  printf 'GENERATED_AT=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'TARGET=%s\nMODEL=%s\nMOCKS=%s\nMOCK_FARM_PORTS=%s\n' "$TARGET" "$MODEL" "$MOCKS" "$FARM_N"
  printf 'KEEP_BODY=%s\nKEEP_SAMPLE=%s\nCHUNK_MAX=%s\n' "$KEEP_BODY" "${KEEP_SAMPLE:-A/E=1.0,B=0.5,C/F=0.1,D=0.05}" "$CHUNK_MAX"
  printf 'GRACEFUL_STOP=%s\nSTREAM_TIMEOUT=%s\nGRACE_MARGIN_S=%s\n' "$GRACEFUL_STOP" "$STREAM_TIMEOUT" "$GRACE_MARGIN_S"
  printf 'K6_MEM=%s\nHOST_MEM_TOTAL_G=%s\n' "$K6_MEM" "${HOST_MEM_TOTAL_G:-unknown}"
  printf 'GW_HOST=%s\nBASELINE_CONN=%s\nMAX_WAIT_SEC=%s\n' "$GW_HOST" "$BASELINE_CONN" "$MAX_WAIT_SEC"
  printf 'K6_IMAGE=%s\nK6_SCRIPT_SHA256=%s\n' "$K6_IMAGE" "$K6_SHA"
  printf 'K6_IMAGE_ID=%s\n' "$(docker image inspect "$K6_IMAGE" --format '{{.Id}}' 2>/dev/null || echo unknown)"
  printf 'COMBOS=%s\nSTART_VUS=%s\nSTEP=%s\nMAX_VUS=%s\n' "$COMBOS" "$START_VUS" "$STEP" "$MAX_VUS"
  printf 'V88_DUAL_MAX_VUS=%s\nSTEP_COARSE_ABOVE=%s\nSTEP_COARSE=%s\n' "$V88_DUAL_MAX_VUS" "$STEP_COARSE_ABOVE" "$STEP_COARSE"
  printf 'STABLE_SHORT=%s\nSTABLE_LONG=%s\nWARMUP=%s\n' "$STABLE_SHORT" "$STABLE_LONG" "$WARMUP"
  printf 'RESUME=%s\nSTOP_ON_FAIL=%s\n' "$RESUME" "$STOP_ON_FAIL"
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
  # 失败分类与长度信号（对应 k6 侧新增的 Counter）。`reqs` 是识别「这档只跑了 1/3」的
  # 机械判据；`split(...)` 取的是**冒号之后**的数字（名字里自带 502/503，不能用裸 grep -oE）。
  # `rst` / `dial` 是**日志行**计数（传输层），与 summary 指标互相独立、可交叉核对。
  local reqs s0 s502 s503 soth rst dial
  reqs=$(tail -60 "$log" | grep -E '^    http_reqs' | grep -oE ': [0-9]+' | head -1 | tr -d ': ')
  s0=$(tail -60 "$log" | grep -E '^    status_0_transport' | grep -oE ': *[0-9]+' | head -1 | tr -d ': ')
  s502=$(tail -60 "$log" | grep -E '^    status_502' | grep -oE ': *[0-9]+' | head -1 | tr -d ': ')
  s503=$(tail -60 "$log" | grep -E '^    status_503' | grep -oE ': *[0-9]+' | head -1 | tr -d ': ')
  soth=$(tail -60 "$log" | grep -E '^    non2xx_other' | grep -oE ': *[0-9]+' | head -1 | tr -d ': ')
  rst=$(grep -c 'connection reset by peer' "$log" || true)
  dial=$(grep -c 'dial: i/o timeout' "$log" || true)
  local tps
  tps=$(printf '%s' "$qps" | awk -v o="$out" '{if($1!="") printf "%.0f", $1*o}')
  printf 'fail=%s%% checks_ok=%s | dur p50=%s p90=%s p99=%s | ttft p50=%s p99=%s avg=%s | chunk_int p50=%s p99=%s (samples=%s) | gw_oh p50=%s p99=%s | qps=%s tps=%s | iter=%s | reqs=%s split(t=%s 502=%s 503=%s other=%s) rst=%s dial=%s' \
    "${fail:-?}" "${cok:-?}" "${dur50:-?}" "${dur90:-?}" "${dur99:-?}" "${t50:-?}" "${t99:-?}" "${tav:-?}" \
    "${chunk50:-?}" "${chunk99:-?}" "${csamp:-?}" "${gw50:-?}" "${gw99:-?}" "${qps:-?}" "${tps:-?}" "${iter:-?}" \
    "${reqs:-?}" "${s0:-?}" "${s502:-?}" "${s503:-?}" "${soth:-?}" "${rst:-0}" "${dial:-0}"
}

# ---------------------------------------------------------------- 主循环
run_group() { # <combo> <out> <ti> <stable>
  local combo="$1" out="$2" ti="$3" stable="$4"
  local row vus
  v88_log ">>>>>> 组 $combo 开始（out=$out interval=${ti}ms 稳态=${stable}）"
  printf '[%s] >>>>>> 开始 %s 组 (out=%s ti=%s 稳态=%s, 起点=%sVU) <<<<<<\n' \
    "$(date +%H:%M:%S)" "$combo" "$out" "$ti" "$stable" "$START_VUS" >>"$PROGRESS"

  # 每组分发一次（客户原版同形）——参数化 batch + 回读校验
  v88_farm_configure "$FARM_HOSTS" "$MOCK_TTFT" "$ti" "$out" "$MOCK_BATCH" "" || return 3
  sleep 2   # 客户原版同形

  # 客户原版同形的每组冒烟：经网关打一发并数 `data:` 行（只打印，不中止）
  local sm_log="$RESULT_DIR/${combo}_smoke.log" sm_body="$RESULT_DIR/${combo}_smoke.body" smoke_chunks
  curl -s -N -m 120 -X POST "$TARGET" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"stream\":true,\"stream_options\":{\"include_usage\":true}}" \
    -o "$sm_body" -w 'http=%{http_code} t=%{time_total}s\n' >"$sm_log" 2>&1 || true
  smoke_chunks=$(grep -c '^data:' "$sm_body" 2>/dev/null || true)
  printf '  冒烟: %s chunks=%s\n' "$(cat "$sm_log" 2>/dev/null)" "${smoke_chunks:-0}" | tee -a "$PROGRESS"
  [ "${smoke_chunks:-0}" -gt 0 ] || v88_warn "冒烟拿到 0 个 data: 行（客户原版只打印，这里同样继续）"

  local failed_prev=0
  # 档位由 `ladder_levels` 唯一给出：主循环与计划打印共用同一份枚举，不再各自算步进。
  for vus in $(ladder_levels); do
    if [ -n "$ONLY_LEVELS" ] && ! printf ',%s,' "$ONLY_LEVELS" | grep -q ",$vus,"; then
      continue
    fi

    local log rc reused=0
    if [ "$RESUME" = "1" ] && [ -s "$RESULT_DIR/${combo}_${vus}.log" ] \
       && [ "$(cat "$RESULT_DIR/${combo}_${vus}.rc" 2>/dev/null)" = "0" ]; then
      # 续跑命中：复用旧轮次的日志，不重跑 k6。仍然 wait_drain（上一档可能刚跑完）。
      reused=1
      log="$RESULT_DIR/${combo}_${vus}.log"
      rc=0
      v88_log "组 ${combo} VUS=${vus} ♻ 复用已有结果（RESUME=1），跳过 k6"
    else
      wait_drain   # 客户原版同形：每档开跑前排空残余连接
      local sample
      sample=$(combo_sample "$combo")
      v88_log "组 ${combo} VUS=${vus} 开始（采样=${sample}）"
      log=$(run_k6_once "$combo" "$vus" "$stable" "$out" "$ti" "$sample" "$CHUNK_MAX")
      rc=$(cat "$RESULT_DIR/${combo}_${vus}.rc" 2>/dev/null || echo 1)
    fi

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
      # 并把降级**记为口径变更**，不静默。
      local down_sample="0.01"
      v88_warn "k6 退出码 ${rc}（疑似 OOM/被 kill）⇒ 降采样重试一次：KEEP_SAMPLE=${down_sample} CHUNK_MAX=16"
      printf '[%s] ⚠️ %s VUS=%s k6 rc=%s → 降采样重试（口径变更，KEEP_SAMPLE=%s）\n' \
        "$(date +%H:%M:%S)" "$combo" "$vus" "$rc" "$down_sample" >>"$PROGRESS"
      log=$(run_k6_once "$combo" "$vus" "$stable" "$out" "$ti" "$down_sample" "16")
      rc=$(cat "$RESULT_DIR/${combo}_${vus}.rc" 2>/dev/null || echo 1)
      printf 'DOWNGRADED_LEVELS=%s_%s\n' "$combo" "$vus" >>"$LEDGER"
      if [ "$rc" != "0" ]; then
        v88_warn "降采样后 k6 仍然 rc=$rc ⇒ 这一档记为 INVALID 并跳到下一组"
        printf '[%s] ✗ %s VUS=%s k6 两次失败 rc=%s ⇒ INVALID\n' "$(date +%H:%M:%S)" "$combo" "$vus" "$rc" >>"$PROGRESS"
        break
      fi
    fi

    local summary mark=""
    summary=$(parse_log "$log" "$out")
    [ "$reused" = "1" ] && mark="♻ "
    printf '[%s] %s%s VUS=%s %s\n' "$(date +%H:%M:%S)" "$mark" "$combo" "$vus" "$summary" | tee -a "$PROGRESS"

    # 首次成功的档做一次「新列是否真的有数」的自检。k6 由 `-v $V88_DIR:/k6` 挂载，
    # 文件身份已由 preflight 的封条保证；这里只是确认 k6 真把 Counter 输出了，
    # 免得跑完几十小时才发现 `split(...)` 全是 `?`。
    if [ "$NEWCOL_CHECKED" != "1" ] && [ "$rc" = "0" ]; then
      NEWCOL_CHECKED=1
      case "$summary" in
        *'split(t=?'*)
          v88_warn "失败分类 Counter 未输出（split 全为 ?）—— 现场 k6.js 可能不是本版本，先核对再继续"
          printf '[%s] ⚠️ 自检：split 全为 ?（k6.js 版本可疑）\n' "$(date +%H:%M:%S)" >>"$PROGRESS" ;;
        *)
          v88_log "自检：失败分类 Counter 已生效" ;;
      esac
    fi

    # 停测规则（STOP_ON_FAIL，默认 0 = **不停**）：过载段那几档的 fail 本身就是结论，
    # 提前停掉只会留下缺口、事后无法解释。置 1 恢复客户原版「连续两档 fail>1% → STOP」。
    # 判定仍用 k6 内置 `http_req_failed`（与客户同口径），不改指标定义。
    local fail over failed_note
    fail=$(tail -50 "$log" | grep 'http_req_failed' | grep -oE '[0-9.]+%' | head -1 | tr -d '%')
    over=0
    [ -n "$fail" ] && over=$(awk -v f="$fail" 'BEGIN{print (f>1)?1:0}')
    if [ "$over" = "1" ]; then
      if [ "$STOP_ON_FAIL" = "1" ]; then
        failed_note="（STOP_ON_FAIL=1：再超一档即停）"
      else
        failed_note="（STOP_ON_FAIL=0，继续跑）"
      fi
      printf '[%s] ⚠️ %s VUS=%s fail=%s%% >1%%%s\n' \
        "$(date +%H:%M:%S)" "$combo" "$vus" "$fail" "$failed_note" >>"$PROGRESS"
      if [ "$STOP_ON_FAIL" = "1" ]; then
        if [ "$failed_prev" = "1" ]; then
          v88_log "组 $combo 连续两档 >1%（VUS=$vus fail=${fail}%）→ STOP"
          break
        fi
        failed_prev=1
      fi
    else
      failed_prev=0
    fi

  done
  v88_log ">>>>>> 组 $combo 完成（阶梯走完，上限 ${MAX_VUS}）"
}

# ---- 计划：档数与墙钟先摆出来，别让人跑到四十小时才发现 ----------------------
# 按**实际会跑的**组与档算：`--combo` 只跑那一组、`--levels` 只跑命中的档。
# （此前这两处都按完整集合算，于是 `--combo B --levels 1000,5000,10000` 会打印
#  “组 B D E F A C 各 20 档 ⇒ 共 120 档”，而实际是 1 组 3 档。）
effective_combos=""
for c in $COMBOS; do
  [ -z "$ONLY_COMBO" ] || [ "$c" = "$ONLY_COMBO" ] || continue
  effective_combos="$effective_combos $c"
done
effective_combos=${effective_combos# }
if [ -n "$ONLY_LEVELS" ]; then
  effective_levels=""
  for v in $(ladder_levels); do
    printf ',%s,' "$ONLY_LEVELS" | grep -q ",$v," && effective_levels="$effective_levels $v"
  done
  effective_levels=${effective_levels# }
  ladder_n=$(printf '%s\n' $effective_levels | grep -c .)
else
  effective_levels=""
  ladder_n=$(ladder_levels | wc -l | tr -d '[:space:]')
fi
group_n=$(printf '%s\n' $effective_combos | grep -c .)
total_n=$((ladder_n * group_n))
nominal_s=0
for c in $effective_combos; do
  row=$(combo_row "$c")
  [ -n "$row" ] || continue
  len=$(printf '%s' "$row" | cut -d'|' -f3)
  st="$STABLE_SHORT"
  if [ "$len" = "LONG" ]; then st="$STABLE_LONG"; fi
  sts=$(printf '%s' "$st" | tr -d 's')
  nominal_s=$((nominal_s + ladder_n * (WARMUP + sts)))
done
{
  printf 'LADDER_LEVELS_PER_GROUP=%s\nLADDER_LEVELS_TOTAL=%s\n' "$ladder_n" "$total_n"
  printf 'ONLY_COMBO=%s\nONLY_LEVELS=%s\n' "${ONLY_COMBO:-}" "${ONLY_LEVELS:-}"
  printf 'EFFECTIVE_COMBOS=%s\nEFFECTIVE_LEVELS_PER_GROUP=%s\n' "$effective_combos" "$effective_levels"
} >>"$LEDGER"
printf '[%s] 阶梯计划：组 %s 各 %s 档 ⇒ 共 %s 档；载荷窗口合计 ≈ %s 小时（真实墙钟更长：每档还要加排空与农场配置）\n' \
  "$(date +%H:%M:%S)" "$effective_combos" "$ladder_n" "$total_n" \
  "$(awk -v s="$nominal_s" 'BEGIN{printf "%.1f", s/3600}')" | tee -a "$PROGRESS"
if [ -n "$ONLY_LEVELS" ]; then
  printf '[%s] --levels 生效：只跑 %s（共 %s 档；已记入 env.txt）\n' \
    "$(date +%H:%M:%S)" "$effective_levels" "$ladder_n" | tee -a "$PROGRESS"
fi
if [ "$STEP_COARSE_ABOVE" -gt 0 ]; then
  printf '[%s] ⚠️ 口径变更：VU ≥ %s 起用步进 %s（比客户阶梯稀疏，已记入 env.txt）\n' \
    "$(date +%H:%M:%S)" "$STEP_COARSE_ABOVE" "$STEP_COARSE" | tee -a "$PROGRESS"
fi
if [ "$RESUME" = "1" ]; then
  printf '[%s] ♻ RESUME=1：已有 rc=0 的档将复用旧日志、不再跑 k6\n' "$(date +%H:%M:%S)" | tee -a "$PROGRESS"
fi

printf '[%s] ====== V88 xg 版开始 | 组 %s | 农场 %s 口 | %sVU 起 +%s | 上限 %s ======\n' \
  "$(date +%H:%M:%S)" "$effective_combos" "$FARM_N" "$START_VUS" "$STEP" "$MAX_VUS" | tee -a "$PROGRESS"
if [ -n "${KEEP_SAMPLE:-}" ]; then
  printf '口径：KEEP_BODY=%s KEEP_SAMPLE=%s(全局固定) CHUNK_MAX=%s gracefulStop=%s | k6.js sha256=%s\n' \
    "$KEEP_BODY" "$KEEP_SAMPLE" "$CHUNK_MAX" "$GRACEFUL_STOP" "$K6_SHA" | tee -a "$PROGRESS"
else
  printf '口径：KEEP_BODY=%s 采样=客户分组(A/E=100%% B=50%% C/F=10%% D=5%%) CHUNK_MAX=%s gracefulStop=%s | k6.js sha256=%s\n' \
    "$KEEP_BODY" "$CHUNK_MAX" "$GRACEFUL_STOP" "$K6_SHA" | tee -a "$PROGRESS"
fi

if [ "$STOP_ON_FAIL" = "1" ]; then
  printf '停测规则：连续两档 fail>1%% → STOP（STOP_ON_FAIL=1，客户原版）\n' | tee -a "$PROGRESS"
else
  printf '停测规则：不停测，跑到 MAX_VUS（STOP_ON_FAIL=0）；过载档只标 ⚠️ 不截断\n' | tee -a "$PROGRESS"
fi

if [ "$DRY_RUN" = "1" ]; then
  v88_log "dry-run：计划如上；**未下发农场、未跑 k6**"
  for c in $COMBOS; do
    [ -z "$ONLY_COMBO" ] || [ "$c" = "$ONLY_COMBO" ] || continue
    row=$(combo_row "$c")
    [ -n "$row" ] || v88_die "未知组 ${c}"
    printf '  组 %s: out=%s interval=%sms sample=%s\n' "$c" \
      "${row%%|*}" "$(printf '%s' "$row" | cut -d'|' -f2)" "$(combo_sample "$c")"
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
