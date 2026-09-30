#!/usr/bin/env bash
# ============================================================
# pool_sizing_probe.sh —— 压测**进行中**，量出「每口连接数 / 每口在途流数」，判定
# `XLITE_UPSTREAM_STREAM_CAP` 到底有没有在绑定，并给出可以照着部署的两个值。
#
# ## 为什么需要它
#
# 2026-09-30 实验室两轮实测（24 口农场 / 16000 VU / 60s / MAX_CONNS=25，每档都过
# 「POOL_ERR=0 + 单流预检 200」闸门）：
#
#   cap=30   ttft p50 771.4 / 767.0ms   dur p50 6.97/6.99s   dur p90 10.04/10.05s   iter/s 1702/1699
#   cap=60   ttft p50 650.0ms           dur p50 6.67s         dur p90 11.19s         1668
#   cap=125  ttft p50 580.0 / 584.8ms   dur p50 6.33/6.49s   dur p90 12.26/14.01s   1637/1522
#   cap=250  ttft p50 549.3 / 547.9ms   dur p50 7.32/7.35s   dur p90 12.24/12.36s   1575/1564
#
# ⇒ 降 CAP 不是"没收益"，而是**拿中位首字换尾部与吞吐**。可它**只在真的绑定**时才
# 有代价：`每连接可用流数 = min(对端 SETTINGS, CAP)`，池子没塞满 CAP 时，改它一个
# 数都不会变。**所以"该不该改"必须先在客户环境量出来，而不是照搬实验室的值。**
#
# 探针只做两件事：量 + 算。**只读**：不修改系统、不重启容器、不写任何配置。
#
# ## 判据（一句话）
#
#   实际每连接流数 ≈ CAP    ⇒ 在绑定 ⇒ 放开 CAP 能拿回首字（代价见上表右列）
#   实际每连接流数 << CAP   ⇒ 没绑定 ⇒ 改 CAP 收益为零，去别处找首字
#
# ## 在途怎么量
#
# k6 的场景是**闭环**：每个 VU 同一时刻只有一个请求在途，所以
# **网关的入站 established 连接数 ≈ 在途并发**（无 sleep 的脚本下成立）。
# 用 `--k6-report` 可以事后交叉校验：`在途 ≈ iterations/s × dur p50`，两者应同量级。
# 若你更信任别的口径，用 `--inflight N` 直接覆盖。
#
# ## 一条拓扑注意
#
# 它按「目标端口在 8081-8104 区间」统计**本机所有进程**的出站连接。所以在**网关机**上跑
# 是准的（那台机只有网关在连农场）。若把网关和农场挤在同一台机上（实验室常见），
# 别的进程/别的网关的常驻连接也会被算进来 —— 那种情况请只信"每口连接数"的量级，
# 并以 --k6-report / --inflight 校准在途。
#
# ## 退出码
#
#   0 = 判定有效；2 = 无负载/负载太低，判定不可信；3 = 缺工具；4 = 每口采样不可用，拒绝判定
# ============================================================
set -uo pipefail

CONTAINER="xlite-gateway"
GW_PORT=8080
FARM_LO=8081
FARM_HI=8104
RUN_SECONDS=120
INTERVAL=5
PEER_CAP=250
# 建议连接数 = 需求峰值 / 对端值 × 余量。余量取 1.5（x10 存整数，避免浮点）。
HEADROOM_X10=15
INFLIGHT_OVERRIDE=""
K6_REPORT=""
OUT_TSV=""
SELF_TEST=0
# 这三个必须在**任何分支之前**给默认值：(1) `set -u` 下只在某分支赋值的变量会在另一
# 分支 unbound 崩掉（实测两次：ceiling_min/peer_min（评审修的）、today_cap/today_conns）；
# (2) 放在聚合之后会把 API 分支刚读到的值抹回 0（同一处踩过第二次）⇒ 只在这里给。
today_cap=0
today_conns=0
# 配置是否**真的读到了**（`/api/pool` 或容器 env）。读不到时不能对外说 "CAP 未设" ——
# 那是把"看不见"说成"没有"。
cfg_known=0
GW_URL=""
ADMIN_PASSWORD_CLI="${ADMIN_PASSWORD:-}"
# `UNIT` 是连接数的单位（API 模式是 host，回落模式是口）。**必须在自测之前就有默认值**：
# `recommend()` 会把它写进理由，而 `--self-test` 在采样区（真正按 API_MODE 赋值的
# 地方）之前就调用它 ⇒ 少了这一行，`set -u` 下自测会在「建议」段直接
# `UNIT: unbound variable` 退出。
UNIT="口"

usage() {
    cat <<'USAGE'
用法：pool_sizing_probe.sh [选项]      （压测进行中，在**网关机**上跑；.24 / .26 各一次）

  --container NAME   网关容器名（默认 xlite-gateway；用来读 env / fd / 启动行）
  --gw-port N        网关监听端口（入站统计，默认 8080）
  --farm-lo N        农场端口范围起点（默认 8081）
  --farm-hi N        农场端口范围终点（默认 8104）
  --seconds N        采样时长（默认 120）
  --interval N       采样间隔（默认 5）
  --peer-cap N       对端通告的 SETTINGS_MAX_CONCURRENT_STREAMS（默认 250，见 CONFIG.md §5.1）
  --headroom N       连接数余量 ×10（默认 15 = 1.5×）
  --inflight N       直接指定在途并发（不给则用入站连接数）
  --k6-report PATH   k6 摘要日志，用于事后交叉校验在途
  --out PATH         原始采样 TSV 路径（默认 /tmp/pool_sizing_probe_<pid>.tsv）
  --gw-url URL       网关地址（默认 http://127.0.0.1:<--gw-port>；走 /api/pool 用）
  --admin-password P 管理员密码：给了就**优先读 /api/pool**（0.5.2+，精确到每条 host
                     的连接数与在途流数，不需要 ss / docker）；不给则回落 ss 采样。
                     ⚠️ 推荐改用 `ADMIN_PASSWORD` 环境变量（脚本优先读它）：`-P`/`P` 两个
                     参数形式都会把值留在本进程 argv（ps / shell history）里。
  --self-test        只跑纯函数自测（不采样）
  -h, --help         显示本帮助
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --container) CONTAINER="$2"; shift 2 ;;
        --gw-port) GW_PORT="$2"; shift 2 ;;
        --farm-lo) FARM_LO="$2"; shift 2 ;;
        --farm-hi) FARM_HI="$2"; shift 2 ;;
        --seconds) RUN_SECONDS="$2"; shift 2 ;;
        --interval) INTERVAL="$2"; shift 2 ;;
        --peer-cap) PEER_CAP="$2"; shift 2 ;;
        --headroom) HEADROOM_X10="$2"; shift 2 ;;
        --inflight) INFLIGHT_OVERRIDE="$2"; shift 2 ;;
        --k6-report) K6_REPORT="$2"; shift 2 ;;
        --out) OUT_TSV="$2"; shift 2 ;;
        --gw-url) GW_URL="$2"; shift 2 ;;
        --admin-password) ADMIN_PASSWORD_CLI="$2"; shift 2 ;;
        --self-test) SELF_TEST=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "未知参数：$1（--help 看用法）" >&2; exit 2 ;;
    esac
done

# ============================================================
# 纯函数区（可自测，不碰系统）
# ============================================================

ceil_div() { # ceil_div <a> <b>  ⇒ ⌈a/b⌉（b≤0 时给 0）
    if [ "${2:-0}" -le 0 ]; then echo 0; return; fi
    echo $(( ($1 + $2 - 1) / $2 ))
}

# binding_verdict <achieved_x10> <cap>  ⇒ 写全局 VERDICT
binding_verdict() {
    local achieved_x10="$1" cap="$2"
    if [ "${cap}" -le 0 ]; then VERDICT="peer-limited"; return; fi
    if [ "${achieved_x10}" -ge $(( cap * 10 * 9 / 10 )) ]; then
        VERDICT="binding"
    elif [ "${achieved_x10}" -le $(( cap * 10 / 2 )) ]; then
        VERDICT="not-binding"
    else
        VERDICT="partial"
    fi
}

# recommend <verdict> <today_cap> <today_conns> <peer_cap> <peak_per_port_inflight> <headroom_x10>
# ⇒ 写全局 RECOMMEND_CAP / RECOMMEND_CAP_ENV / RECOMMEND_CONNS / RECOMMEND_CONNS_ENV / RECOMMEND_REASON
recommend() {
    local verdict="$1" today_cap="$2" today_conns="$3" peer="$4" peak_per_port="$5" hr="$6"
    local demand ceiling_from_today

    # 配置未知时**不给出具体建议**：today_cap/today_conns 是默认值 0，落到下面的通用路径会算出
    # “保住今天的上界 0×0”这种假数字，而判定明明说的是“配置未知” —— 把看不见说成有，
    # 现场还会照抄那段部署块。与 `unusable` 的处理对齐。
    if [ "${verdict}" = "unknown-config" ]; then
        RECOMMEND_CAP="（配置未知）"
        RECOMMEND_CAP_ENV="（配置未知）"
        RECOMMEND_CONNS="（配置未知）"
        RECOMMEND_CONNS_ENV="（配置未知）"
        RECOMMEND_REASON="当前 CAP/CONNS 都没读到（既无 /api/pool 也无容器 env）⇒ 不猜，先恢复配置可见性再采样"
        return
    fi

    if [ "${verdict}" = "not-binding" ]; then
        RECOMMEND_CAP="保持现状（$( [ "${today_cap}" = 0 ] && echo '未设' || echo "${today_cap}" )）"
        RECOMMEND_CAP_ENV="$( [ "${today_cap}" = 0 ] && echo '<保持未设>' || echo "${today_cap}" )"
        RECOMMEND_CONNS="保持现状（${today_conns}）"
        RECOMMEND_CONNS_ENV="${today_conns}"
        RECOMMEND_REASON="实际每连接流数远低于 CAP ⇒ 池子没塞满，改 CAP 一个数都不会变；首字问题不在这条轴上"
        return
    fi

    RECOMMEND_CAP="不设（跟随对端，常为 ${peer}）"
    RECOMMEND_CAP_ENV="<删除此变量>"
    # 需求：峰值每口在途 ÷ 对端值 × 余量
    demand=$(ceil_div $(( peak_per_port * hr )) $(( peer * 10 )))
    [ "${demand}" -lt 2 ] && demand=2
    # 保住今天的上界：today_conns × today_cap 用 peer 口径表达
    if [ "${today_cap}" -gt 0 ]; then
        ceiling_from_today=$(ceil_div $(( today_conns * today_cap )) "${peer}")
    else
        ceiling_from_today=2
    fi
    [ "${ceiling_from_today}" -lt 2 ] && ceiling_from_today=2
    if [ "${demand}" -ge "${ceiling_from_today}" ]; then
        RECOMMEND_CONNS_ENV="${demand}"
    else
        RECOMMEND_CONNS_ENV="${ceiling_from_today}"
    fi
    RECOMMEND_CONNS="${RECOMMEND_CONNS_ENV}"
    RECOMMEND_REASON="需求峰值 ${peak_per_port}/${UNIT} ÷ ${peer}（对端值）× 余量 $(awk -v h="${hr}" 'BEGIN{printf "%.1f", h/10}') ⇒ ${demand}；保住今天的上界 ${today_conns}×${today_cap} 需要用 ${ceiling_from_today} 条 ⇒ 取大者 ${RECOMMEND_CONNS_ENV}"
}

# 解析我们自己的 /api/pool 负载。字段顺序由 serde 的 BTreeMap 决定 ⇒ **逐字段抽取**，不假设顺序。
# 输出：每 host 一行 "<host> <connections> <in_flight> <effective_ceiling> <peer_cap>"（后两列可为空）
api_hosts_from_json() {
    # 注意两点：(1) `hosts` 不一定排在最后（serde 的 BTreeMap 把 max_conns_per_host /
    # stream_budget_per_conn 排在它后面）⇒ 只截到**第一个** `]`；
    # (2) 用 awk 的 `RS='},{'` 切记录 —— **BSD sed 的替换里 `\n` 不是换行**，
    # `sed 's/},{/}\n{/g'` 在 macOS 上会把两个 host 粘成一个（实测丢过第二个）。
    printf '%s' "$1" | sed -n 's/.*"hosts":\[//p' | sed 's/\].*//' | awk -v RS='},{' '{
        # 每轮重置**所有**列（漏一个就会沿用上一个 host 的值 —— 上一版踩过 e/p）。
        h=""; c=""; f=""; e=""; p=""; w=""; q=""; n99="";
        if (match($0, /"host":"[^"]*"/))      { h = substr($0, RSTART + 8,  RLENGTH - 9) }
        if (match($0, /"connections":[0-9]+/)){ c = substr($0, RSTART + 14, RLENGTH - 14) }
        if (match($0, /"in_flight":[0-9]+/))  { f = substr($0, RSTART + 12, RLENGTH - 12) }
        if (match($0, /"peer_cap":[0-9]+/))   { p = substr($0, RSTART + 11, RLENGTH - 11) }
        if (match($0, /"effective_ceiling":[0-9]+/)) { e = substr($0, RSTART + 20, RLENGTH - 20) }
        if (match($0, /"waiting":[0-9]+/))      { w = substr($0, RSTART + 10, RLENGTH - 10) }
        if (match($0, /"queued_total":[0-9]+/)) { q = substr($0, RSTART + 15, RLENGTH - 15) }
        if (match($0, /"wait_ms_p99":[0-9]+/))  { n99 = substr($0, RSTART + 14, RLENGTH - 14) }
        # 后三列统一给数字：waiting / queued_total 缺席时本来就是 0；wait_ms_p99 在
        # "没排过队"时无意义，靠 queued_total==0 判断，不靠这一列。
        # 可缺席的两列用占位符 `-`：留空会把后面三列**挤位**（$5 变成 0），
        # 而"上界/对端未知"与"等于 0"是两回事。聚合侧本来就只认数字，`-` 会被跳过。
        if (h != "") print h, (c == "" ? 0 : c), (f == "" ? 0 : f), \
                            (e == "" ? "-" : e), (p == "" ? "-" : p), \
                            (w == "" ? 0 : w), (q == "" ? 0 : q), (n99 == "" ? 0 : n99)
    }'
}

# ============================================================
# 自测（纯函数；不采样、不需要压测）
# ============================================================
if [ "${SELF_TEST}" = "1" ]; then
    fails=0
    check() { # check <名称> <实际> <期望>
        if [ "$2" != "$3" ]; then echo "  ✗ $1：实际 '$2'，期望 '$3'"; fails=$((fails+1));
        else echo "  ✓ $1"; fi
    }
    echo "== ceil_div =="
    check "750/250" "$(ceil_div 750 250)" "3"
    check "751/250" "$(ceil_div 751 250)" "4"
    check "0/250" "$(ceil_div 0 250)" "0"
    check "x/0" "$(ceil_div 5 0)" "0"

    echo "== 判定 =="
    binding_verdict 300 30; check "30/30 ⇒ binding" "${VERDICT}" "binding"
    binding_verdict 280 30; check "28/30 ⇒ binding" "${VERDICT}" "binding"
    binding_verdict 150 30; check "15/30 ⇒ not-binding" "${VERDICT}" "not-binding"
    binding_verdict 200 30; check "20/30 ⇒ partial" "${VERDICT}" "partial"
    binding_verdict 200 0;  check "cap 未设 ⇒ peer-limited" "${VERDICT}" "peer-limited"

    echo "== 建议 =="
    recommend binding 30 25 250 450 15
    check "现场 45000（每口 450 在途）⇒ CAP" "${RECOMMEND_CAP_ENV}" "<删除此变量>"
    check "现场 45000 ⇒ CONNS" "${RECOMMEND_CONNS_ENV}" "3"
    recommend binding 30 25 250 225 15
    check "现场 dual 半载（每口 225）⇒ CONNS" "${RECOMMEND_CONNS_ENV}" "3"
    recommend binding 30 25 250 1800 15
    check "极载（每口 1800）⇒ CONNS" "${RECOMMEND_CONNS_ENV}" "11"
    recommend not-binding 30 25 250 40 15
    check "未绑定 ⇒ CAP 保持" "${RECOMMEND_CAP_ENV}" "30"
    check "未绑定 ⇒ CONNS 保持" "${RECOMMEND_CONNS_ENV}" "25"
    check "未绑定 ⇒ 理由" "${RECOMMEND_REASON}" "实际每连接流数远低于 CAP ⇒ 池子没塞满，改 CAP 一个数都不会变；首字问题不在这条轴上"

    echo "== /api/pool 解析 =="
    # 关键：serde 的 BTreeMap 按字典序输出（cap_updates < connections < dials < host <
    # in_flight < …）⇒ 解析器必须**逐字段抽取**、不假设键顺序。下面第二个 host 故意把
    # in_flight 写在前面，顺序与真实负载相反。
    fixture='{"data":{"hosts":[{"cap_updates":0,"connections":2,"dials":7,"effective_ceiling":750,"effective_streams_per_conn":250,"host":"10.56.88.29:8081","in_flight":443,"peer_cap":250,"queued_total":34,"reclaimed":1,"settings_fallbacks":0,"wait_ms_p99":4096,"waiting":394},{"connections":3,"effective_ceiling":750,"host":"10.56.88.29:8082","in_flight":750,"peer_cap":250}],"max_conns_per_host":3,"stream_budget_per_conn":30},"success":true}'
    check "解析 2 host（8 列；第 2 个缺席的队列列必须为 0）" "$(api_hosts_from_json "${fixture}" | tr '\n' '|')" \
        "10.56.88.29:8081 2 443 750 250 394 34 4096|10.56.88.29:8082 3 750 750 250 0 0 0|"
    empty='{"data":{"hosts":[],"max_conns_per_host":3,"stream_budget_per_conn":null},"success":true}'
    check "空 hosts ⇒ 零输出" "$(api_hosts_from_json "${empty}" | grep -c . || true)" "0"
    check "cap 数字可抽到" "$(printf '%s' "${fixture}" | sed -n 's/.*"stream_budget_per_conn":\([0-9]*\).*/\1/p')" "30"
    check "cap=null ⇒ 抽不到数字（= 随对端）" "$(printf '%s' "${empty}" | sed -n 's/.*"stream_budget_per_conn":\([0-9]*\).*/\1/p')" ""
    check "max_conns 可抽到" "$(printf '%s' "${fixture}" | sed -n 's/.*"max_conns_per_host":\([0-9]*\).*/\1/p')" "3"
    # 回归：`peer_cap`/`effective_ceiling` 为 null 的 host（未握手/无活连接）**不能沿用**
    # 上一个 host 的值 —— awk 逐字段抽取时忘了每轮重置 `e`/`p` 就会残留。
    mixed='{"data":{"hosts":[{"connections":2,"effective_ceiling":750,"host":"h1","in_flight":1,"peer_cap":250},{"connections":3,"host":"h2","in_flight":2}],"max_conns_per_host":3,"stream_budget_per_conn":30},"success":true}'
    # 同时钉两件事：不沿用上一个 host 的值（残留），也不把后面的列挤位（`-` 占位）。
    check "null 上界/对端不残留且不挤位（第2个 host）" "$(api_hosts_from_json "${mixed}" | sed -n '2p' | awk '{print $4"/"$5"/"$6"/"$7"/"$8}')" "-/-/0/0/0"

    if [ "${fails}" -gt 0 ]; then echo "自测失败 ${fails} 项"; exit 1; fi
    echo "自测全通过"
    exit 0
fi

# ============================================================
# 采样区
# ============================================================
# 数据来源二选一：
#   A) /api/pool（给 --admin-password；0.5.2+ 的网关）—— 精确读到每条 host 的连接数与
#      在途流数，不需要 ss，也不需要 docker。
#   B) ss + 入站连接数（回落路径，与旧版一致）—— 本机到农场的出站连接，入站连接当在途上界。
GW_URL="${GW_URL:-http://127.0.0.1:${GW_PORT}}"
API_MODE=0
[ -n "${ADMIN_PASSWORD_CLI}" ] && API_MODE=1
command -v awk >/dev/null 2>&1 || { echo "缺工具：awk" >&2; exit 3; }
command -v sort >/dev/null 2>&1 || { echo "缺工具：sort" >&2; exit 3; }
if [ "${API_MODE}" = 1 ]; then
    command -v curl >/dev/null 2>&1 || { echo "缺工具：curl（走 /api/pool 需要；或去掉 --admin-password 回落 ss）" >&2; exit 3; }
else
    command -v ss >/dev/null 2>&1 || { echo "缺工具：ss（回落路径需要；或给 --admin-password 走 /api/pool）" >&2; exit 3; }
fi

TSV="${OUT_TSV:-/tmp/pool_sizing_probe_$$.tsv}"
PORTS="${TSV%.tsv}.ports.tsv"
API="${TSV%.tsv}.api.tsv"
OUT_FILTER="( dport >= :${FARM_LO} and dport <= :${FARM_HI} )"
IN_FILTER="( sport = :${GW_PORT} )"
rounds=$(( RUN_SECONDS / INTERVAL ))
[ "${rounds}" -lt 1 ] && rounds=1

api_json=""; api_hosts=""; api_cap=""; api_maxconns=""; api_has_queue=0
fetch_pool_api() { # 登录换 cookie，再取每 host 快照；成功返回 0
    local jar; jar="$(mktemp)"
    # 密码经 stdin（--data @-）送给 curl，**不走 curl 的 argv** —— 否则 `ps` 能看到
    # `curl ... -d {"password":"…"}`。脚本自己的 argv 仍可能带 --admin-password，
    # 所以文档推荐用 ADMIN_PASSWORD 环境变量（见 usage）。
    printf '{"password":"%s"}' "${ADMIN_PASSWORD_CLI}" \
        | curl -sS -m 5 -c "${jar}" -X POST "${GW_URL}/api/auth/login" \
            -H 'Content-Type: application/json' --data @- \
            -o /dev/null 2>/dev/null || { rm -f "${jar}"; return 1; }
    api_json="$(curl -sS -m 5 -b "${jar}" "${GW_URL}/api/pool" 2>/dev/null)"
    rm -f "${jar}"
    api_cap="$(printf '%s' "${api_json}" | sed -n 's/.*"stream_budget_per_conn":\([0-9]*\).*/\1/p')"
    printf '%s' "${api_json}" | grep -q '"stream_budget_per_conn":null' && api_cap="null"
    api_maxconns="$(printf '%s' "${api_json}" | sed -n 's/.*"max_conns_per_host":\([0-9]*\).*/\1/p')"
    # 这个端点的版本差异：0.5.2 的 /api/pool 没有队列字段。缺字段时**不能**把
    # "看不见"当成"没有队列"（否则会打印"0 个在等"，那是假事实）。
    api_has_queue=0
    printf '%s' "${api_json}" | grep -q '"waiting"' && api_has_queue=1
    case "${api_json}" in *'"hosts"'*) return 0 ;; *) return 1 ;; esac
}
api_ok=0; api_note=""; api_cfg=0
UNIT="口"; [ "${API_MODE}" = 1 ] && UNIT="host"
if [ "${API_MODE}" = 1 ]; then
    if fetch_pool_api; then
        api_hosts="$(api_hosts_from_json "${api_json}")"
        if [ -n "${api_hosts}" ]; then api_ok=1; else api_note="（/api/pool 可达但 hosts 为空 —— 此刻没有上游流量？）"; fi
    else
        api_note="（/api/pool 不可用：登录失败，或该版本还没有这个端点）"
    fi
fi

# API 模式在**运行时**才可能回落到 ss（登录失败 / 端点缺失 / hosts 为空），所以 ss 只在
# 确定要走回落路径时才检查 —— 否则会把“不需要 ss”的 API 模式也卡住。没有 ss 时给清晰
# 报错，而不是静默采到 0 条、再报一个误导性的“每口连接数不可用”。
if [ "${api_ok}" != 1 ]; then
    command -v ss >/dev/null 2>&1 || { echo "缺工具：ss（/api/pool 不可用${api_note} ⇒ 需要回落 ss 路径；修好密码/地址，或装上 iproute2）" >&2; exit 3; }
fi

: > "${TSV}"; : > "${PORTS}"; : > "${API}"
if [ "${api_ok}" = 1 ]; then
    echo "== 数据来源：/api/pool（精确）；采样 ${rounds} 次 × ${INTERVAL}s ==" >&2
else
    echo "== 数据来源：ss + 入站连接数 ${api_note}；采样 ${rounds} 次 × ${INTERVAL}s ==" >&2
fi
i=0
while [ "${i}" -lt "${rounds}" ]; do
    i=$((i + 1))
    ts=$(date +%s)
    if [ "${api_ok}" = 1 ]; then
        # 每轮重取一次，跨轮取每 host 峰值 —— 单点快照会漏掉尖峰
        if fetch_pool_api; then
            api_hosts="$(api_hosts_from_json "${api_json}")"
            printf '%s\n' "${api_hosts}" >> "${API}"
            n=$(printf '%s\n' "${api_hosts}" | grep -c .)
            c=$(printf '%s\n' "${api_hosts}" | awk '{s+=$2} END{print s+0}')
            f=$(printf '%s\n' "${api_hosts}" | awk '{s+=$3} END{print s+0}')
            printf '\r  轮 %d/%d  host %s  连接 %s  在途 %s      ' "${i}" "${rounds}" "${n}" "${c}" "${f}" >&2
        else
            printf '\r  轮 %d/%d  /api/pool 本轮取数失败      ' "${i}" "${rounds}" >&2
        fi
    else
        ss -Htan state established "${OUT_FILTER}" 2>/dev/null > "${TSV}.ss"
        out_total=$(wc -l < "${TSV}.ss" | tr -d ' ')
        in_total=$(ss -Htan state established "${IN_FILTER}" 2>/dev/null | wc -l | tr -d ' ')
        printf '%s\t%s\t%s\n' "${ts}" "${out_total}" "${in_total}" >> "${TSV}"
        # 每口连接数：把本轮的分布记成 "<轮次> <端口> <连接数>"，跨轮取每口最大值
        # ⚠️ `ss -Htan` 的实际列布局是 `Recv-Q Send-Q Local Peer` —— **没有 State 列**（实测 4 列），
        # 所以取 $NF（Peer），不能写死 $5（写了就会把"总数"当成端口）。
        awk -v s="${i}" '{split($NF, a, ":"); p = a[length(a)]; c[p]++} END {for (p in c) print s, p, c[p]}' \
            "${TSV}.ss" >> "${PORTS}"
        printf '\r  轮 %d/%d  出站 %s   入站 %s      ' "${i}" "${rounds}" "${out_total}" "${in_total}" >&2
    fi
    [ "${i}" -lt "${rounds}" ] && sleep "${INTERVAL}"
done
printf '\n' >&2
rm -f "${TSV}.ss"

# ---- 汇总 ----
# 回落路径（无 --admin-password）不会进 API 分支 ⇒ 这些量必须先有默认值：
# `set -u` 下连 `[ -n "${ceiling_min}" ]` 都会 unbound（#68 引入、本 PR 修复的回归），
# 否则文档承诺的「无密码时优雅降级」实际是整个探针在回落模式下崩掉。
ceiling_min=""; peer_min=""
waiting_total=0; queued_total=0; wait_p99_max=0
if [ "${api_ok}" = 1 ]; then
    # 五列都要带上：后两列（准入上界 / 对端通告值）是给报告与建议用的，
    # 只留前三列会让它们凭空消失（实测踩过）。
    awk '{h=$1
          if ($2 > cm[h]) cm[h] = $2
          if ($3 > fm[h]) fm[h] = $3
          if ($4 ~ /^[0-9]+$/) ce[h] = $4
          if ($5 ~ /^[0-9]+$/) pc[h] = $5
          if ($6 > wm[h]) wm[h] = $6
          if ($7 > qm[h]) qm[h] = $7
          if ($8 > pm[h]) pm[h] = $8}
         END {for (h in cm) print h, cm[h], fm[h], ce[h], pc[h], wm[h], qm[h], pm[h]}' \
        "${API}" | sort > "${PORTS}.max"
    n_ports=$(wc -l < "${PORTS}.max" | tr -d ' ')
    [ "${n_ports}" -gt 0 ] || n_ports=1
    per_port_max=$(awk '{if ($2 > m) m = $2} END {print m+0}' "${PORTS}.max")
    per_port_median=$(awk '{print $2}' "${PORTS}.max" | sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : 0)}')
    conns_total=$(awk '{s+=$2} END{print s+0}' "${PORTS}.max")
    # 准入上界与对端通告值：跨 host 取**最小**（保守）。上界 = min(CAP, 对端) × CONNS，
    # 由网关算好直接给出 ⇒ 这里不再需要手算，也不再靠 --peer-cap 猜。
    ceiling_min=$(awk '$4 ~ /^[0-9]+$/ {if (m == "" || $4 < m) m = $4} END {print m}' "${PORTS}.max")
    peer_min=$(awk '$5 ~ /^[0-9]+$/ {if (m == "" || $5 < m) m = $5} END {print m}' "${PORTS}.max")
    # 队列：深度跨 host 求和（每 host 取窗口内峰值），等待分位取最坏的那个 host。
    waiting_total=$(awk '{s+=$6} END{print s+0}' "${PORTS}.max")
    queued_total=$(awk '{s+=$7} END{print s+0}' "${PORTS}.max")
    wait_p99_max=$(awk '{if ($8 > m) m = $8} END{print m+0}' "${PORTS}.max")
    inflight_total=$(awk '{s+=$3} END{print s+0}' "${PORTS}.max")
    out_peak="${conns_total}"; out_med="${conns_total}"
    in_peak="${inflight_total}"; in_med="${inflight_total}"
    if [ "${api_cap}" = "null" ]; then today_cap=0; else today_cap="${api_cap:-0}"; fi
    today_conns="${api_maxconns:-0}"
    api_cfg=1
    cfg_known=1
else
    out_peak=$(awk -F'\t' '{if($2>m)m=$2} END{print m+0}' "${TSV}")
    out_med=$(awk -F'\t' '{print $2}' "${TSV}" | sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : 0)}')
    in_peak=$(awk -F'\t' '{if($3>m)m=$3} END{print m+0}' "${TSV}")
    in_med=$(awk -F'\t' '{print $3}' "${TSV}" | sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : 0)}')
    # 每口在窗口内的最大值
    awk '{if ($3 > m[$2]) m[$2] = $3} END {for (p in m) print p, m[p]}' "${PORTS}" | sort -n > "${PORTS}.max"
    n_ports=$(wc -l < "${PORTS}.max" | tr -d ' ')
    [ "${n_ports}" -gt 0 ] || n_ports=1
    per_port_max=$(awk '{if ($2 > m) m = $2} END {print m+0}' "${PORTS}.max")
    per_port_median=$(awk '{print $2}' "${PORTS}.max" | sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : 0)}')
fi

# ---- 当前配置（读容器 env；读不到就降级）----
# 默认值在文件顶部的默认值区给（那里在 API 与 docker 两条分支**之前**）——
# 在这里再初始化一次就会抹掉 API 分支刚读到的 today_cap/today_conns（实测踩过）。
startup=""
gw_limits=""
if [ "${api_cfg}" = 1 ]; then
    :  # 配置来自 /api/pool（today_cap/today_conns 已填好，**不要**在这里重置）
elif command -v docker >/dev/null 2>&1 && docker inspect "${CONTAINER}" >/dev/null 2>&1; then
    envs=$(docker inspect "${CONTAINER}" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null)
    cap_raw=$(printf '%s\n' "${envs}" | sed -n 's/^XLITE_UPSTREAM_STREAM_CAP=//p')
    conns_raw=$(printf '%s\n' "${envs}" | sed -n 's/^XLITE_UPSTREAM_MAX_CONNS_PER_HOST=//p')
    case "${cap_raw}" in ''|*[!0-9]*) today_cap=0 ;; *) today_cap="${cap_raw}" ;; esac
    case "${conns_raw}" in ''|*[!0-9]*) today_conns=0 ;; *) today_conns="${conns_raw}" ;; esac
    cfg_known=1
    startup=$(docker logs "${CONTAINER}" 2>&1 | grep -m1 'starting' || true)
    gw_limits=$(docker exec "${CONTAINER}" sh -c \
        "grep -i 'open files' /proc/1/limits; printf 'fds_used='; ls /proc/1/fd 2>/dev/null | wc -l" 2>/dev/null || true)
fi

# ---- 在途：口径优先级 = --inflight > k6 摘要（qps × dur p50）> 入站连接数 ----
inflight_total="${in_med}"
inflight_meter="入站连接数（在途的**上界** —— VU 在两次请求之间也占着连接）"
if [ "${api_ok}" = 1 ]; then inflight_meter="由 /api/pool 直接读数（精确，非上界）"; fi
k6_derived=""
if [ -n "${K6_REPORT}" ] && [ -r "${K6_REPORT}" ]; then
    k6_qps=$(grep -m1 'iterations' "${K6_REPORT}" 2>/dev/null | grep -oE '[0-9.]+/s' | tr -d '/s')
    k6_dur=$(grep -m1 'http_req_duration' "${K6_REPORT}" 2>/dev/null | grep -oE 'med=[0-9.]+(ms|s)' | cut -d= -f2)
    if [ -n "${k6_qps}" ] && [ -n "${k6_dur}" ]; then
        k6_derived=$(awk -v q="${k6_qps}" -v d="${k6_dur}" 'BEGIN{
            if (d ~ /ms$/) { sub(/ms$/, "", d); d = d / 1000 } else { sub(/s$/, "", d) }
            printf "%d", q * d }')
    fi
fi
if [ -n "${k6_derived}" ] && [ "${k6_derived:-0}" -gt 0 ]; then
    inflight_total="${k6_derived}"
    inflight_meter="k6 摘要校准（iterations/s × dur p50）"
fi
if [ -n "${INFLIGHT_OVERRIDE}" ]; then
    inflight_total="${INFLIGHT_OVERRIDE}"
    inflight_meter="--inflight 指定"
fi
per_port_inflight=$(awk -v t="${inflight_total}" -v n="${n_ports}" 'BEGIN{printf "%d", (n? t/n : 0)}')
achieved_x10=0
if [ "${per_port_median}" -gt 0 ]; then
    achieved_x10=$(awk -v t="${per_port_inflight}" -v c="${per_port_median}" 'BEGIN{printf "%d", t*10/c}')
fi

data_ok=1
min_ports=2; [ "${api_ok}" = 1 ] && min_ports=1
if [ "${per_port_median:-0}" -le 0 ] || [ "${n_ports:-0}" -lt "${min_ports}" ]; then
    # 每口连接数不可用 ⇒ **拒绝判定**。宁可让人重采，也不能给一个自信的错结论。
    data_ok=0
fi
if [ "${data_ok}" = "1" ]; then
    if [ "${cfg_known}" = 0 ]; then
        # 连配置都没读到（既没有 /api/pool 也没有容器 env）⇒ 不要猜 CAP 设没设。
        VERDICT="unknown-config"
    else
        binding_verdict "${achieved_x10}" "${today_cap}"
    fi
    peak_for_sizing=$(( per_port_max > per_port_inflight ? per_port_max : per_port_inflight ))
    peer_for_sizing="${PEER_CAP}"
    if [ -n "${peer_min}" ]; then peer_for_sizing="${peer_min}"; fi
    recommend "${VERDICT}" "${today_cap}" "${today_conns}" "${peer_for_sizing}" "${peak_for_sizing}" "${HEADROOM_X10}"
else
    VERDICT="unusable"
    RECOMMEND_CAP="（拒绝判定）"
    RECOMMEND_CAP_ENV="（拒绝判定）"
    RECOMMEND_CONNS="（拒绝判定）"
    RECOMMEND_CONNS_ENV="（拒绝判定）"
    RECOMMEND_REASON="每${UNIT}连接数不可用（跨${UNIT}中位 ${per_port_median:-空} / 活跃${UNIT}数 ${n_ports:-0}）"
fi

# 上界的来源（写清是怎么算出来的，免得又变成"看不见的数"）
ceiling_hint=""
used_pct="（未知）"
if [ -n "${ceiling_min}" ]; then
    ceiling_hint="（min(CAP=${today_cap:-未设}, 对端=${peer_min:-?}) × CONNS=${today_conns:-?}）"
    used_pct=$(awk -v u="${per_port_inflight}" -v c="${ceiling_min}" 'BEGIN{ if (c > 0) printf "%d%%", u*100/c; else print "（未知）" }')
fi
# 对端值比我们的 CAP 还小时，指出"真正限流的是对端" —— 否则读者会把
# `not-binding` 误读成"池子没塞满"，而实际上是我们的 CAP 根本没机会绑定。
# 队列非空 = 上界正被钉住（队排在上游之外），代价记在等待上 —— 这正是"上游很窄"时的
# 现场形态，所以要把等待分位一起报出来。
queue_line="（未知，需 /api/pool）"
if [ "${api_ok}" = 1 ] && [ "${api_has_queue}" = 0 ]; then
    queue_line="（该版本的 /api/pool 还没有队列字段）"
elif [ "${api_ok}" = 1 ]; then
    queue_line="${waiting_total} 个在等 / 累计 ${queued_total}"
fi
queue_hint=""
if [ -n "${waiting_total}" ] && [ "${waiting_total:-0}" -gt 0 ]; then
    if [ "${queued_total:-0}" -gt 0 ]; then
        queue_hint="  ← 队列非空，等待 p99 ≤ ${wait_p99_max}ms（上界被钉住，代价记在等待上）"
    queue_line="${queue_line}${queue_hint}"
    else
        queue_hint="  ← 队列非空（尚无排完队的样本，故没有等待分位）"
        queue_line="${queue_line}${queue_hint}"
    fi
fi
peer_hint=""
if [ -n "${peer_min}" ] && [ -n "${today_cap}" ] && [ "${today_cap}" != 0 ] && [ "${peer_min}" -lt "${today_cap}" ]; then
    peer_hint="  ⚠️ 对端只允许 ${peer_min} 流/连接 < 你的 CAP=${today_cap} ⇒ 你的 CAP 没机会绑定，限流的是对端"
fi
in_label="入站（k6→网关）"
out_label="出站到农场  "
out_hint="（端口 ${FARM_LO}-${FARM_HI}）"
ports_label="活跃农场口数"
detail_label="每口连接数明细"
conns_label="每口连接数     "
inflight_label="每口在途流数   "
if [ "${api_ok}" = 1 ]; then
    conns_label="每 host 连接数 "
    inflight_label="每 host 在途流数"
    in_label="池内在途（Σhost）"
    out_label="池内连接（Σhost）"
    out_hint="（不按端口聚合：/api/pool 按 host 报）"
    ports_label="活跃 host 数 "
    detail_label="每 host 连接/在途明细"
fi
case "${VERDICT}" in
    binding)      verdict_cn="**在绑定** —— 放开 CAP 会把中位首字还回来，代价是尾部与吞吐" ;;
    partial)      verdict_cn="**部分绑定** —— 改 CAP 会有变化，但幅度小于实验室的满绑定场景" ;;
    not-binding)  verdict_cn="**没绑定** —— 改 CAP 一个数都不会变，去别处找首字" ;;
    peer-limited) verdict_cn="CAP 未设，池子跟随对端通告（这本身是推荐状态）" ;;
    unknown-config) verdict_cn="**配置未知** —— 既没读到「/api/pool」，也没读到容器 env（需要 docker 或 --admin-password）" ;;
    unusable)     verdict_cn="**拒绝判定** —— 每口连接数不可用，请重采（见下）" ;;
esac

# ---- k6 事后交叉校验（可选）----
k6_line=""
if [ -n "${K6_REPORT}" ] && [ -r "${K6_REPORT}" ]; then
    k6_line=$(grep -E 'http_req_duration' "${K6_REPORT}" | head -1)
fi

cat <<EOF
# ============ pool_sizing_probe 观测（$(date -Iseconds)）============
容器              : ${CONTAINER}
当前 env          : STREAM_CAP=$( [ "${today_cap}" = 0 ] && echo '(未设)' || echo "${today_cap}" )  MAX_CONNS=$( [ "${today_conns}" = 0 ] && echo '(未设)' || echo "${today_conns}" )
启动行            : ${startup:-（未读到）}
容器 fd           : ${gw_limits:-（未读到，非 docker 场景可忽略）}

窗口              : ${rounds} 轮 × ${INTERVAL}s
${out_label}        : 峰值 ${out_peak}，中位 ${out_med}${out_hint}
${in_label}   : 峰值 ${in_peak}，中位 ${in_med}   ← 在途口径：${inflight_meter}
${ports_label}      : ${n_ports}
${conns_label}  : 峰值 ${per_port_max}，跨口中位 ${per_port_median}
${inflight_label}  : ≈ ${per_port_inflight}
准入上界（每 host）: ${ceiling_min:-（未知，需 /api/pool）} 流${ceiling_hint}${peer_hint}
排队（当前/累计） : ${queue_line}
已用（每 host 在途峰值/上界） : ${used_pct}
实际每连接流数    : ≈ $(awk -v a="${achieved_x10}" 'BEGIN{printf "%.1f", a/10}')

判定              : ${VERDICT} —— ${verdict_cn}
建议 STREAM_CAP   : ${RECOMMEND_CAP}
建议 MAX_CONNS    : ${RECOMMEND_CONNS}
理由              : ${RECOMMEND_REASON}

${detail_label}（窗口内最大值，前 8）：
EOF
if [ "${api_ok}" = 1 ]; then
    sort -k2 -rn "${PORTS}.max" | head -8 | awk '{printf "  %s：%s 连接 / %s 在途\n", $1, $2, $3}'
else
    sort -k2 -rn "${PORTS}.max" | head -8 | awk '{printf "  端口 %s：%s 条\n", $1, $2}'
fi

if [ -n "${k6_line}" ]; then
    cat <<EOF

k6 交叉校验       : ${k6_line}
                    （在途 ≈ iterations/s × dur p50，应与上面的入站中位同量级）
EOF
fi

if [ "${data_ok}" != "1" ]; then
    {
        echo
        echo "⚠️  每${UNIT}连接数不可用（跨${UNIT}中位 ${per_port_median:-空}，活跃${UNIT}数 ${n_ports:-0}）—— **拒绝给出建议**。"
        echo "    原始采样：${TSV} / ${PORTS}.max"
        echo "    常见原因：(a) 采样窗口内没有压测在跑（池子只剩常驻连接）；(b) ss 输出格式与本脚本预期不符。"
        echo "    请在上表列出的目标档位跑着的时候重采。"
    } >&2
    exit 4
fi

if [ "${cfg_known}" = 1 ]; then
cat <<EOF

# ============ 复制到部署（⚠️ .24 与 .26 必须一致）============
# XLITE_UPSTREAM_STREAM_CAP=${RECOMMEND_CAP_ENV}
# XLITE_UPSTREAM_MAX_CONNS_PER_HOST=${RECOMMEND_CONNS_ENV}
# docker restart 不会改 env，必须按原命令 / compose 重建容器；
# 改完先空跑一轮 warmup 再计入数据。

# ============ 写进报告的「口径变更」说明（照抄）============
本轮把 XLITE_UPSTREAM_STREAM_CAP 由 $( [ "${today_cap}" = 0 ] && echo '未设' || echo "${today_cap}" ) 调整为「${RECOMMEND_CAP_ENV}」，
XLITE_UPSTREAM_MAX_CONNS_PER_HOST 由 ${today_conns} 调整为 ${RECOMMEND_CONNS_ENV}；
依据为只读探针 advanced/pool_sizing_probe.sh 的现场采样（$(basename "${TSV}")），未改动系统配置。
EOF
else
cat <<EOF

# ============ 无从给出部署建议（配置未知）============
# 本轮既没读到 /api/pool，也没读到容器 env ⇒ 不知道现在的 CAP/CONNS 是多少，
# 也就无法给出「保住现有上界」的调整建议。请改用 --admin-password（走 /api/pool），
# 或在有 docker、能看到容器 ${CONTAINER} 的机器上重跑。
EOF
fi

# 负载是否"低到不可信"：ss 路径看入站连接（≈ 在途 VU），API 路径看**队列** ——
# API 模式下 in_med 是"池内在途"，它被上界钉住，饱和的小上界（如 7/口）看起来就像
# 没负载（实测踩过：2529 个请求在排队，却报"负载太低"）。有队列/排过队 ⇒ 显然有负载。
low_load=0
if [ "${in_med}" -lt 100 ]; then low_load=1; fi
if [ "${api_ok}" = 1 ] && { [ "${waiting_total:-0}" -gt 0 ] || [ "${queued_total:-0}" -gt 0 ]; }; then
    low_load=0
fi
if [ "${low_load}" = 1 ]; then
    {
        echo
        if [ "${api_ok}" = 1 ]; then
        echo "⚠️  窗口内既没有排队、池内在途中位也只有 ${in_med} —— 看不出负载，判定不可信。"
    else
        echo "⚠️  窗口内入站连接中位只有 ${in_med} —— 负载太低，池子没被压满，判定不可信。"
    fi
        echo "    请在**目标档位**（如 45000 / 55000 VU）跑着的时候再采一次。"
    } >&2
    exit 2
fi
exit 0
