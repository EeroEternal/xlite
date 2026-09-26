#!/usr/bin/env bash
# 自证：仓库自带 Node mock（scripts/stream_mock_server.mjs）的 `_config` 契约。
#
# 为什么需要它：三个 harness（run_levels.sh / ab_configs.sh / ab_topology.sh）都靠
# `POST /_config` 读写农场参数，而 `_config` 的**回读形态**是有隐含约定的 ——
#   * 读回 JSON 里 `"batch":N` 后面必须**紧跟 `}`**：harness 用
#     `grep -o '"batch":[0-9]*}'` 取值，键序一变就读不到，会被判成「配置没生效」
#     并 fail-closed 退出（这正是不该发生的假 blocker）。
#   * `batch=N` 必须**不改载荷一个字节**：只改 write 次数。
# 这两条只要有一条被改坏，压测结论就会静默错，所以做成可跑的自证。
#
# 用法：
#   scripts/loadtest/test_stream_mock_config.sh
# 退出码：0 = 全过；1 = 有用例失败。
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
MOCK="$ROOT/scripts/stream_mock_server.mjs"
PORT_A=${PORT_A:-4199} # batch=1
PORT_B=${PORT_B:-4198} # batch=5
PIDS=""
FAILS=0
PASSES=0

cleanup() {
  for pid in $PIDS; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT

ok() {
  PASSES=$((PASSES + 1))
  printf '  \033[32m✓\033[0m %s\n' "$1"
}
no() {
  FAILS=$((FAILS + 1))
  printf '  \033[31m✗\033[0m %s\n' "$1" >&2
  [ $# -gt 1 ] && printf '      期望：%s\n      实得：%s\n' "$2" "$3" >&2
  return 0
}
eq() { # <名称> <期望> <实得>
  if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "$2" "$3"; fi
}

command -v node >/dev/null 2>&1 || { echo "✗ 需要 node" >&2; exit 1; }
command -v curl >/dev/null 2>&1 || { echo "✗ 需要 curl" >&2; exit 1; }

for port in "$PORT_A" "$PORT_B"; do
  if curl -s -m 1 -o /dev/null "http://127.0.0.1:$port/health" 2>/dev/null; then
    echo "✗ 端口 $port 已被占用，换 PORT_A/PORT_B 再跑" >&2
    exit 1
  fi
done

start_mock() { # <port> <batch>
  node "$MOCK" --port "$1" --listen 127.0.0.1 --model mock \
    --chunks 20 --interval-ms 0 --ttft-ms 0 --batch "$2" --label "selftest-$1" \
    >"/tmp/selftest-$1.log" 2>&1 &
  PIDS="$PIDS $!"
  for _ in $(seq 1 40); do
    curl -s -m 1 -o /dev/null "http://127.0.0.1:$1/health" 2>/dev/null && return 0
    sleep 0.1
  done
  echo "✗ mock :$1 起不来，看 /tmp/selftest-$1.log" >&2
  exit 1
}

readback() { curl -s -m 3 -X POST "http://127.0.0.1:$1/_config"; }
write_cfg() { curl -s -m 3 -X POST "http://127.0.0.1:$1/_config?$2"; }
code_of() { curl -s -m 3 -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$1/_config?$2"; }
stream() { # <port> → 响应体
  curl -s -m 10 -H 'Content-Type: application/json' \
    -d '{"model":"mock","stream":true,"messages":[{"role":"user","content":"hi"}]}' \
    "http://127.0.0.1:$1/v1/chat/completions"
}
health_writes() { # <port> → 累计 write 次数
  curl -s -m 3 "http://127.0.0.1:$1/health" | grep -o '"writes":[0-9]*' | tr -dc '0-9'
}

echo "== 1. 回读形态（harness 的 grep/case 约定）=="
start_mock "$PORT_A" 1
start_mock "$PORT_B" 5

body=$(readback "$PORT_A")
# harness 的原样惯用法：必须有 `"batch":<digits>}`，即 batch 是最后一个键
got=$(printf '%s' "$body" | grep -o '"batch":[0-9]*}' | tr -dc '0-9' || true)
eq "grep -o '\"batch\":[0-9]*}' 能取到值" "1" "$got"

case "$body" in
  *'"batch":1}'*) ok "case 模式 *\"batch\":1}* 命中（run_levels/ab_configs 用的就是它）" ;;
  *) no "case 模式 *\"batch\":1}* 命中" '命中' '未命中' ;;
esac

# 反向守卫：如果哪天有人把 batch 挪到中间（后面还有别的键），这个用例必须红
if printf '%s' "$body" | grep -qE '"batch":[0-9]+\}$'; then
  ok "batch 是 JSON 的最后一个键"
else
  no "batch 是 JSON 的最后一个键" '…"batch":N}' "…"$body"…"
fi

echo "== 2. 写入 + 回读（harness 原样查询串）=="
write_cfg "$PORT_A" "ttft=0&token_interval=0&out_tokens=500&mix=0&mode=2&batch=5" >/dev/null
got=$(readback "$PORT_A" | grep -o '"batch":[0-9]*}' | tr -dc '0-9' || true)
eq "RUN_LEVELS 那一串查询串写入后回读到 batch=5" "5" "$got"
got=$(readback "$PORT_A" | grep -o '"out_tokens":[0-9]*' | tr -dc '0-9' || true)
eq "同一串里的 out_tokens 也生效" "500" "$got"
write_cfg "$PORT_A" "batch=1" >/dev/null
got=$(readback "$PORT_A" | grep -o '"batch":[0-9]*}' | tr -dc '0-9' || true)
eq "只写 batch（ab_* 的还原路径）" "1" "$got"

echo "== 3. 静默失效必须变成显式 400 =="
eq "不认识的键" "400" "$(code_of "$PORT_A" 'bogus=1')"
eq "非整数" "400" "$(code_of "$PORT_A" 'batch=abc')"
eq "越界 batch=0" "400" "$(code_of "$PORT_A" 'batch=0')"
eq "越界 out_tokens=0" "400" "$(code_of "$PORT_A" 'out_tokens=0')"
eq "本 mock 不兑现的 mix≠0" "400" "$(code_of "$PORT_A" 'mix=3')"
eq "本 mock 不兑现的 mode≠2" "400" "$(code_of "$PORT_A" 'mode=1')"
eq "合法的整串仍是 200" "200" "$(code_of "$PORT_A" 'mix=0&mode=2&batch=1')"

echo "== 4. 配置对**后续请求**生效 =="
write_cfg "$PORT_A" "out_tokens=3&token_interval=0&batch=1" >/dev/null
got=$(stream "$PORT_A" | grep -c '^data: ')
eq "out_tokens=3 ⇒ 3 个内容块 + finish + [DONE] = 5 行 data:" "5" "$got"

echo "== 5. batch 不改载荷（字节数 / data: 行数 / [DONE]）=="
write_cfg "$PORT_A" "out_tokens=20&token_interval=0&batch=1" >/dev/null
write_cfg "$PORT_B" "out_tokens=20&token_interval=0&batch=5" >/dev/null
sa=$(stream "$PORT_A")
sb=$(stream "$PORT_B")
eq "响应字节数相同" "${#sa}" "${#sb}"
eq "data: 行数相同" "$(printf '%s' "$sa" | grep -c '^data: ')" "$(printf '%s' "$sb" | grep -c '^data: ')"
eq "都恰好 1 个 [DONE]" "$(printf '%s' "$sa" | grep -c '\[DONE\]')" "$(printf '%s' "$sb" | grep -c '\[DONE\]')"
case "$sa" in
  *'"ts"'*) ok "载荷里带 ts（gw_oh 依赖它）" ;;
  *) no "载荷里带 ts（gw_oh 依赖它）" '有 ts' '没有' ;;
esac

echo "== 6. batch 真的减少了 write（不是只靠读代码断言）=="
# 20 个内容块 + 1 个 finish + 1 个 [DONE] = 22 次 emit
w0=$(health_writes "$PORT_A")
stream "$PORT_A" >/dev/null
w1=$(health_writes "$PORT_A")
eq "batch=1 ⇒ 22 帧 = 22 次 write" "22" "$((w1 - w0))"
w0=$(health_writes "$PORT_B")
stream "$PORT_B" >/dev/null
w1=$(health_writes "$PORT_B")
eq "batch=5 ⇒ 同样 22 帧，ceil(22/5) = 5 次 write" "5" "$((w1 - w0))"

echo
if [ "$FAILS" -eq 0 ]; then
  printf '\033[32m✓ test_stream_mock_config: %d 项全过\033[0m\n' "$PASSES"
  exit 0
fi
printf '\033[31m✗ test_stream_mock_config: %d 项失败（%d 项通过）\033[0m\n' "$FAILS" "$PASSES" >&2
exit 1
