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
    RECOMMEND_REASON="需求峰值 ${peak_per_port}/口 ÷ ${peer}（对端值）× 余量 $(awk -v h="${hr}" 'BEGIN{printf "%.1f", h/10}') ⇒ ${demand}；保住今天的上界 ${today_conns}×${today_cap} 需要用 ${ceiling_from_today} 条 ⇒ 取大者 ${RECOMMEND_CONNS_ENV}"
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

    if [ "${fails}" -gt 0 ]; then echo "自测失败 ${fails} 项"; exit 1; fi
    echo "自测全通过"
    exit 0
fi

# ============================================================
# 采样区
# ============================================================
for tool in ss awk sort; do
    command -v "${tool}" >/dev/null 2>&1 || { echo "缺工具：${tool}" >&2; exit 3; }
done

TSV="${OUT_TSV:-/tmp/pool_sizing_probe_$$.tsv}"
PORTS="${TSV%.tsv}.ports.tsv"
OUT_FILTER="( dport >= :${FARM_LO} and dport <= :${FARM_HI} )"
IN_FILTER="( sport = :${GW_PORT} )"
rounds=$(( RUN_SECONDS / INTERVAL ))
[ "${rounds}" -lt 1 ] && rounds=1

: > "${TSV}"; : > "${PORTS}"
echo "== 采样 ${rounds} 次 × ${INTERVAL}s（无压测在跑时退出码会是 2，这是故意的）==" >&2
i=0
while [ "${i}" -lt "${rounds}" ]; do
    i=$((i + 1))
    ts=$(date +%s)
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
    [ "${i}" -lt "${rounds}" ] && sleep "${INTERVAL}"
done
printf '\n' >&2
rm -f "${TSV}.ss"

# ---- 汇总 ----
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

# ---- 当前配置（读容器 env；读不到就降级）----
today_cap=0
today_conns=0
startup=""
gw_limits=""
if command -v docker >/dev/null 2>&1 && docker inspect "${CONTAINER}" >/dev/null 2>&1; then
    envs=$(docker inspect "${CONTAINER}" --format '{{range .Config.Env}}{{println .}}{{end}}' 2>/dev/null)
    cap_raw=$(printf '%s\n' "${envs}" | sed -n 's/^XLITE_UPSTREAM_STREAM_CAP=//p')
    conns_raw=$(printf '%s\n' "${envs}" | sed -n 's/^XLITE_UPSTREAM_MAX_CONNS_PER_HOST=//p')
    case "${cap_raw}" in ''|*[!0-9]*) today_cap=0 ;; *) today_cap="${cap_raw}" ;; esac
    case "${conns_raw}" in ''|*[!0-9]*) today_conns=0 ;; *) today_conns="${conns_raw}" ;; esac
    startup=$(docker logs "${CONTAINER}" 2>&1 | grep -m1 'starting' || true)
    gw_limits=$(docker exec "${CONTAINER}" sh -c \
        "grep -i 'open files' /proc/1/limits; printf 'fds_used='; ls /proc/1/fd 2>/dev/null | wc -l" 2>/dev/null || true)
fi

# ---- 在途：口径优先级 = --inflight > k6 摘要（qps × dur p50）> 入站连接数 ----
inflight_total="${in_med}"
inflight_meter="入站连接数（在途的**上界** —— VU 在两次请求之间也占着连接）"
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
if [ "${per_port_median:-0}" -le 0 ] || [ "${n_ports:-0}" -lt 2 ]; then
    # 每口连接数不可用 ⇒ **拒绝判定**。宁可让人重采，也不能给一个自信的错结论。
    data_ok=0
fi
if [ "${data_ok}" = "1" ]; then
    binding_verdict "${achieved_x10}" "${today_cap}"
    peak_for_sizing=$(( per_port_max > per_port_inflight ? per_port_max : per_port_inflight ))
    recommend "${VERDICT}" "${today_cap}" "${today_conns}" "${PEER_CAP}" "${peak_for_sizing}" "${HEADROOM_X10}"
else
    VERDICT="unusable"
    RECOMMEND_CAP="（拒绝判定）"
    RECOMMEND_CAP_ENV="（拒绝判定）"
    RECOMMEND_CONNS="（拒绝判定）"
    RECOMMEND_CONNS_ENV="（拒绝判定）"
    RECOMMEND_REASON="每口连接数不可用（跨口中位 ${per_port_median:-空} / 活跃口数 ${n_ports:-0}）"
fi

case "${VERDICT}" in
    binding)      verdict_cn="**在绑定** —— 放开 CAP 会把中位首字还回来，代价是尾部与吞吐" ;;
    partial)      verdict_cn="**部分绑定** —— 改 CAP 会有变化，但幅度小于实验室的满绑定场景" ;;
    not-binding)  verdict_cn="**没绑定** —— 改 CAP 一个数都不会变，去别处找首字" ;;
    peer-limited) verdict_cn="CAP 未设，池子跟随对端通告（这本身是推荐状态）" ;;
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
出站到农场        : 峰值 ${out_peak}，中位 ${out_med}（端口 ${FARM_LO}-${FARM_HI}）
入站（k6→网关）   : 峰值 ${in_peak}，中位 ${in_med}   ← 在途口径：${inflight_meter}
活跃农场口数      : ${n_ports}
每口连接数        : 峰值 ${per_port_max}，跨口中位 ${per_port_median}
每口在途流数      : ≈ ${per_port_inflight}
实际每连接流数    : ≈ $(awk -v a="${achieved_x10}" 'BEGIN{printf "%.1f", a/10}')

判定              : ${VERDICT} —— ${verdict_cn}
建议 STREAM_CAP   : ${RECOMMEND_CAP}
建议 MAX_CONNS    : ${RECOMMEND_CONNS}
理由              : ${RECOMMEND_REASON}

每口连接数明细（窗口内最大值，前 8 口）：
EOF
sort -k2 -rn "${PORTS}.max" | head -8 | awk '{printf "  端口 %s：%s 条\n", $1, $2}'

if [ -n "${k6_line}" ]; then
    cat <<EOF

k6 交叉校验       : ${k6_line}
                    （在途 ≈ iterations/s × dur p50，应与上面的入站中位同量级）
EOF
fi

if [ "${data_ok}" != "1" ]; then
    {
        echo
        echo "⚠️  每口连接数不可用（跨口中位 ${per_port_median:-空}，活跃口数 ${n_ports:-0}）—— **拒绝给出建议**。"
        echo "    原始采样：${TSV} / ${PORTS}.max"
        echo "    常见原因：(a) 采样窗口内没有压测在跑（池子只剩常驻连接）；(b) ss 输出格式与本脚本预期不符。"
        echo "    请在上表列出的目标档位跑着的时候重采。"
    } >&2
    exit 4
fi

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

if [ "${in_med}" -lt 100 ]; then
    {
        echo
        echo "⚠️  窗口内入站连接中位只有 ${in_med} —— 负载太低，池子没被压满，判定不可信。"
        echo "    请在**目标档位**（如 45000 / 55000 VU）跑着的时候再采一次。"
    } >&2
    exit 2
fi
exit 0
