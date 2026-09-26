#!/usr/bin/env bash
# ============================================================
# deploy/v88 框架共用件：日志 / 哈希 / 封条自检 / 农场控制 / 产能预算。
# 被 `run_full_88_xg.sh` 与 `advanced/farm_ceiling_scan.sh` source，不单独执行。
#
# **离线自足**是硬约束：客户机上只拷 `deploy/v88/` 也必须能跑，所以这里**不** source
# 仓库里的任何路径，只 source 同目录的 `lib_rig.sh`。本包与 `scripts/loadtest/` 之间
# **没有**跨目录校验；身份保护靠自封条 `BUNDLE_SHA256`（见 `seal_bundle.sh` 与 README）。
# ============================================================

V88_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)   # 本文件在 lib/ 下 ⇒ 上一级是包根
# shellcheck source=lib_rig.sh
. "$V88_DIR/lib/lib_rig.sh"

v88_log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
v88_warn() { printf '⚠️  %s\n' "$*" >&2; }
v88_die() { printf '✗ %s\n' "$*" >&2; exit 2; }

v88_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# ---------------------------------------------------------------- 本框架自身 lint
# 本框架的报错文案大量是中文，而 `$var` 后面**紧跟全角字符**时（写成 `$got` 后直接接 `）`），
# bash 会把多字节字节当成变量名的一部分 ⇒ 变量被当成未定义、**静默展开成空**，
# 只在输出里留下半个全角字的乱码。本框架第一版就踩到了：报错文案里的哈希变成两个
# 乱码字节，排查时完全看不出是"变量没展开"。
# 规则：`$name` 后接非 ASCII 时必须写 ${name}。
v88_lint_expansions() {
  local bad=0 f hits
  for f in "$V88_DIR"/*.sh "$V88_DIR"/advanced/*.sh "$V88_DIR"/lib/*.sh; do
    [ -f "$f" ] || continue
    case "${f##*/}" in ._*) continue ;; esac   # macOS 拷贝留下的 AppleDouble 影子文件，别拿去 grep
    hits=$(LC_ALL=C grep -nE '\$[A-Za-z_][A-Za-z0-9_]*[^ -~[:space:]]' "$f" || true)
    if [ -n "$hits" ]; then
      printf '✗ %s：`$var` 后面紧跟非 ASCII 字符，必须写成 ${var}：\n%s\n' "$f" "$hits" >&2
      bad=1
    fi
  done
  [ "$bad" -eq 0 ] || v88_die "本框架自身的 shell 文件没通过变量展开 lint（见上）"
  v88_log "lint：框架内 shell 文件的变量展开写法通过"
}

# ---------------------------------------------------------------- 自封条自检
# 本目录是**自足包**：不与仓库任何路径比对（现场离线没有仓库）。
# 但必须能发现「拷到现场之后被就地改过」——那是 §1.1 的真实故障形态。
# BUNDLE_SHA256 的行格式：`<sha256>  <文件名>`；`#` 开头是注释。
v88_check_bundle() {
  [ -f "$V88_DIR/BUNDLE_SHA256" ] || v88_die "缺 BUNDLE_SHA256 —— 跑 deploy/v88/seal_bundle.sh"
  local h name got n=0
  while read -r h name; do
    case "$h" in ""|"#"*) continue ;; esac
    [ -n "${name:-}" ] || continue
    [ -f "$V88_DIR/$name" ] || v88_die "缺文件 $V88_DIR/$name"
    got=$(v88_sha256 "$V88_DIR/$name")
    if [ "$got" != "$h" ]; then
      v88_die "封条不符：${name}（期望 $h / 实得 ${got}）
   ⇒ 这个文件被就地改过。要么跑 deploy/v88/seal_bundle.sh 重新打封条，要么先解释清楚为什么改。"
    fi
    n=$((n + 1))
  done <"$V88_DIR/BUNDLE_SHA256"
  v88_log "preflight：本目录 $n 个封条文件匹配（自足包，不与仓库比对）"
}

# ---------------------------------------------------------------- 农场主机列表
# `MOCKS` 支持三种写法：`8081-8104`（本机端口区间）、`4101,4102`（本机端口列表）、
# `10.0.0.2:8081-8104` 或 `10.0.0.2:4101,10.0.0.3:4101`（跨机）。
# 输出：一行一个 `host:port`。
v88_farm_hosts() {
  case "$1" in
    [0-9]*-[0-9]*) for p in $(expand_ports "$1"); do printf '127.0.0.1:%s\n' "$p"; done ;;
    *:*) printf '%s' "$1" | tr ',' '\n' | grep -v '^$' ;;
    *) for p in $(expand_ports "$1"); do printf '127.0.0.1:%s\n' "$p"; done ;;
  esac
}

v88_farm_count() { v88_farm_hosts "$1" | grep -c . ; }

# 回读：**必须**用 harness 的原样惯用法（`"batch":N` 后紧跟 `}`）—— 这也是
# `_config` 的隐含契约，改了 JSON 键序就会读到空，从而把「没生效」当成「没读到」。
v88_farm_read_batch() {
  curl -s -m 3 -X POST "http://$1/_config" 2>/dev/null | grep -o '"batch":[0-9]*}' | tr -dc '0-9'
}
v88_farm_read_field() { # <host:port> <json key>
  curl -s -m 3 -X POST "http://$1/_config" 2>/dev/null | grep -o "\"$2\":[0-9]*" | head -1 | tr -dc '0-9'
}
v88_farm_write_batch() { curl -s -m 3 -X POST "http://$1/_config?batch=$2" >/dev/null 2>&1; }

# 一组完整下发（与客户 driver 同形）：ttft / token_interval / out_tokens / mix / mode / batch
v88_farm_apply_group() { # <host:port> <ttft> <interval> <out> <batch>
  curl -s -m 3 -X POST \
    "http://$1/_config?ttft=$2&token_interval=$3&out_tokens=$4&mix=0&mode=2&batch=$5" \
    >/dev/null 2>&1
}

# 逐口下发 + **回读校验** + 记账。全局 V88_FARM_SAVED 保存原值供收尾还原。
# 返回：0 = 全部生效；1 = 有口没生效（调用方**必须**中止）；2 = 一个口都没有（空转）
v88_farm_configure() { # <hosts> <ttft> <interval> <out> <batch> <ledger 可为空>
  local hosts="$1" ttft="$2" interval="$3" out="$4" batch="$5" ledger="${6:-}"
  local h b ok=0 n=0 pending=""
  # 先记原值；只对「原值已知」的口动手 —— 「写过 ⇒ 一定能还原」是不变量
  for h in $hosts; do
    b=$(v88_farm_read_batch "$h")
    if [ -n "$b" ]; then
      pending=$(farm_remember "$pending" "$h" "$b")
    else
      v88_warn ":$h 读不到 batch —— 该口不是支持 _config 的 mock（Go mock-llm:h2c 或本仓 Node mock），或它压根没起来"
    fi
  done
  V88_FARM_SAVED="${pending# }"
  for kv in $pending; do
    v88_farm_apply_group "${kv%%=*}" "$ttft" "$interval" "$out" "$batch"
  done
  for h in $hosts; do
    n=$((n + 1))
    b=$(v88_farm_read_batch "$h")
    o=""
    [ -n "$out" ] && o=$(v88_farm_read_field "$h" out_tokens)
    # 注意括号：`A && (B || C)` 不是 `(A && B) || C` —— 后者会在 out 为空时误判通过
    if [ "$b" = "$batch" ] && { [ -z "$out" ] || [ "$o" = "$out" ]; }; then
      ok=$((ok + 1))
    else
      v88_warn ":$h 回读不符（要 batch=$batch${out:+/out_tokens=$out}，得 batch=${b:-空} out=${o:-未查}）"
    fi
  done
  if [ -n "$ledger" ]; then
    {
      printf 'MOCK_FARM_PORTS=%s\n' "$n"
      printf 'MOCK_BATCH=%s\n' "$batch"
      printf 'MOCK_BATCH_BEFORE=%s\n' "$V88_FARM_SAVED"
    } >>"$ledger"
  fi
  [ "$n" -gt 0 ] || {
    v88_warn "MOCKS=$1 展开后一个口都没有"
    return 2
  }
  if [ "$ok" -ne "$n" ]; then
    v88_warn "农场配置未在所有口生效（$ok/${n}）—— 静默失效会产出假结论"
    return 1
  fi
  v88_log "农场：batch=$batch out=$out interval=$interval ttft=$ttft 生效 $ok/$n 口"
  return 0
}

_config_probe() { curl -s -m 3 -X POST "http://$1/_config" 2>/dev/null | head -c 200; }

# 收尾还原（逐口写回原值）。`kill -9` 下 trap 不执行，所以**每轮都要记账**；
# 只靠这里不够 —— 下一轮开跑前的回读才是「上一轮到底跑的是什么口径」的唯一凭据。
v88_farm_restore() {
  local kv n=0
  for kv in ${V88_FARM_SAVED:-}; do
    v88_farm_write_batch "${kv%%=*}" "${kv#*=}"
    n=$((n + 1))
  done
  [ "$n" -gt 0 ] && v88_log "农场 batch 已按口还原 $n 口：$V88_FARM_SAVED"
  return 0
}

# ---------------------------------------------------------------- 农场产能预算
# 农场成本（**每千请求/秒消耗的核数**）来自本仓实测归一：
#   batch=1  : 34–38   （docs/dev/loadtest-newapi-controlled-2026-09.md §13.3/§13.4）
#   batch=5  : 12.6–14.1（由农场 µs/chunk 72.2→26.8 折算，§14.2）
#   batch=10 : 7.1–10.8（§14.3 四档归一）
# 返回 `pess-opt`：pess = **悲观**所需核数（用来拒绝档位），opt = 乐观。
v88_farm_cores_per_kqps() {
  case "$1" in
    1) echo "38-34" ;;
    5) echo "14-12.6" ;;
    10) echo "11-7.1" ;;
    *) echo "" ;;
  esac
}

# 由 mock 核数 + batch 推出「农场能服务的 qps 上限」：输出 `pess opt`（req/s）
v88_farm_qps_limits() { # <batch> <farm cores>
  local c lo hi
  c=$(v88_farm_cores_per_kqps "$1")
  [ -n "$c" ] || return 1
  lo=${c%%-*}
  hi=${c##*-}
  awk -v cores="$2" -v lo="$lo" -v hi="$hi" \
    'BEGIN{printf "%.0f %.0f", cores*1000/lo, cores*1000/hi}'
}

# 单流时长估计：mock 理想 OUT×interval，实测比理想高约 18%（B 组 5.0s→5.9s）
v88_est_dur_s() { # <out tokens> <interval ms>
  awk -v out="$1" -v ti="$2" 'BEGIN{printf "%.1f", out*ti/1000*1.18}'
}

# 该 batch 下「农场能撑住的 VU 上限」（悲观值；活跃流 ≈ VU）。输出纯数字，供捕获。
v88_farm_vu_cap() { # <batch> <farm cores> <out tokens> <interval ms>
  local lim pess dur
  lim=$(v88_farm_qps_limits "$1" "$2") || return 1
  pess=${lim%% *}
  dur=$(v88_est_dur_s "$3" "$4")
  awk -v q="$pess" -v d="$dur" 'BEGIN{printf "%.0f", q*d}'
}

# 打印产能预算表（**只打印，不回显数字** —— 回显会污染 `$(...)` 捕获）
v88_print_farm_budget() { # <batch> <farm cores> <out tokens> <interval ms>
  local lim pess opt dur cap
  lim=$(v88_farm_qps_limits "$1" "$2") || {
    v88_warn "没有 batch=$1 的农场成本实测值（只测过 1 / 5 / 10）→ 跳过预算闸门"
    return 0
  }
  pess=${lim%% *}
  opt=${lim##* }
  dur=$(v88_est_dur_s "$3" "$4")
  cap=$(v88_farm_vu_cap "$1" "$2" "$3" "$4")
  printf '  农场预算（MOCK_BATCH=%s / %s 核 / 单流≈%ss）：\n' "$1" "$2" "$dur"
  printf '    能服务 %s–%s req/s（悲观–乐观）⇒ VU 上限 ≈ %s（悲观；活跃流≈VU）\n' "$pess" "$opt" "$cap"
  printf '    超过这个档位测的是**农场**，不是网关。要往上打就加 mock 机器，不要靠 k6 硬顶。\n'
}

# 该档位是否超出农场预算。返回 0 = 超了
v88_over_farm_budget() { # <vus> <cap>
  awk -v v="$1" -v c="$2" 'BEGIN{exit !(v+0 > c+0)}'
}
