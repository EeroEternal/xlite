#!/usr/bin/env bash
# ============================================================
# start_farm.sh —— 按 MOCKS（或 MOCK_FIRST/MOCK_LAST）在本机起 / 查 / 生成目录
#
# 改口数只改环境变量，不要改脚本：
#   MOCKS=8081-8116 ./start_farm.sh              # 36 口
#   MOCKS=8081-8112 ./start_farm.sh              # 32 口
#   MOCK_FIRST=8081 MOCK_LAST=8104 ./start_farm.sh
#   MOCKS=10.56.88.24:8081-8116 ./start_farm.sh  # 跨机写法：本机仍按端口起
#   ./start_farm.sh status
#   MOCK_HOST=10.56.88.29 ./start_farm.sh catalog > /tmp/farm.yaml
#   ./start_farm.sh --self-test
#
# 起容器是**幂等**的：已有且在跑的口跳过，停着的 `docker start`，没有的才 `docker run`。
# 缩口数不会删多出来的容器（driver 也不会打它们）。
# ============================================================
set -euo pipefail

V88_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib/lib_v88.sh
. "$V88_DIR/lib/lib_v88.sh"

MOCK_BIN=${MOCK_BIN:-/root/loadtest/mock/mock-llm-linux-usage}
MOCK_IMAGE=${MOCK_IMAGE:-mock-llm:h2c}
MOCK_NAME_PREFIX=${MOCK_NAME_PREFIX:-mock51h-}
MOCK_NOFILE=${MOCK_NOFILE:-1048576}
MOCK_HOST=${MOCK_HOST:-127.0.0.1}
CATALOG_MODEL=${CATALOG_MODEL:-mock}
CATALOG_UPSTREAM_MODEL=${CATALOG_UPSTREAM_MODEL:-mock}
CATALOG_KEY_NAME=${CATALOG_KEY_NAME:-loadtest}
CATALOG_KEY=${CATALOG_KEY:-sk-loadtest}

CMD=start
usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    start|status|catalog) CMD="$1"; shift ;;
    --self-test) CMD=selftest; shift ;;
    -h|--help) usage ;;
    *) v88_die "未知参数 $1（--help）" ;;
  esac
done

resolve_mocks() {
  if [ -n "${MOCKS:-}" ]; then
    return 0
  fi
  if [ -n "${MOCK_FIRST:-}" ] && [ -n "${MOCK_LAST:-}" ]; then
    MOCKS="${MOCK_FIRST}-${MOCK_LAST}"
    return 0
  fi
  if [ -n "${MOCK_FIRST:-}" ] || [ -n "${MOCK_LAST:-}" ]; then
    v88_die "MOCK_FIRST 与 MOCK_LAST 必须成对（或直接设 MOCKS=8081-8116）"
  fi
  v88_die "未设 MOCKS。例：MOCKS=8081-8116（36 口）或 MOCK_FIRST=8081 MOCK_LAST=8112（32 口）。改口数只改这一处，不要改脚本。"
}

# 本机要听的端口。MOCKS 带主机名时只取端口；多个主机直接拒绝（本脚本只起本机容器）。
farm_local_ports() {
  local hosts n_hosts
  hosts=$(v88_farm_hosts "$1" | awk -F: '{print $1}' | sort -u)
  n_hosts=$(printf '%s\n' "$hosts" | grep -c . || true)
  if [ "$n_hosts" -gt 1 ]; then
    v88_die "start_farm.sh 只在本机起容器，MOCKS 不能含多个主机（现在是：$(printf '%s' "$hosts" | tr '\n' ' ')）。本机口数用 MOCKS=8081-8116"
  fi
  v88_farm_hosts "$1" | awk -F: '{print $NF}'
}

farm_port_count() { farm_local_ports "$1" | grep -c . || true; }

emit_catalog() {
  local p
  printf 'providers:\n'
  for p in $(farm_local_ports "$MOCKS"); do
    printf '  - name: mock-%s\n' "$p"
    printf '    base_url: http://%s:%s\n' "$MOCK_HOST" "$p"
    printf '    api_key: ""\n'
    printf '    enabled: true\n'
  done
  printf 'models:\n'
  printf '  - name: %s\n' "$CATALOG_MODEL"
  printf '    enabled: true\n'
  printf '    channels:\n'
  for p in $(farm_local_ports "$MOCKS"); do
    printf '      - provider: mock-%s\n' "$p"
    printf '        upstream_model: %s\n' "$CATALOG_UPSTREAM_MODEL"
    printf '        weight: 1\n'
    printf '        priority: 0\n'
    printf '        enabled: true\n'
  done
  printf 'api_keys:\n'
  printf '  - name: %s\n' "$CATALOG_KEY_NAME"
  printf '    key: %s\n' "$CATALOG_KEY"
  printf '    models: []\n'
  printf '    enabled: true\n'
}

container_name() { printf '%s%s' "$MOCK_NAME_PREFIX" "$1"; }

do_start() {
  local p name n created=0 started=0 skipped=0
  n=$(farm_port_count "$MOCKS")
  [ "$n" -gt 0 ] || v88_die "MOCKS=${MOCKS} 展开后一个口都没有"
  [ -x "$MOCK_BIN" ] || v88_die "找不到可执行的 mock 二进制：${MOCK_BIN}（MOCK_BIN= 可改）"
  command -v docker >/dev/null 2>&1 || v88_die "本机没有 docker"
  v88_log "起农场 ${MOCKS}（${n} 口） image=${MOCK_IMAGE} bin=${MOCK_BIN}"
  for p in $(farm_local_ports "$MOCKS"); do
    name=$(container_name "$p")
    if docker inspect "$name" >/dev/null 2>&1; then
      if [ "$(docker inspect -f '{{.State.Running}}' "$name")" = "true" ]; then
        skipped=$((skipped + 1))
      else
        docker start "$name" >/dev/null
        started=$((started + 1))
      fi
    else
      docker run -d --name "$name" --network host --restart unless-stopped \
        --ulimit nofile="${MOCK_NOFILE}:${MOCK_NOFILE}" \
        -v "${MOCK_BIN}:/mock-llm:ro" \
        --entrypoint /mock-llm -e PORT="$p" "$MOCK_IMAGE" >/dev/null
      created=$((created + 1))
    fi
  done
  v88_log "容器：新建 ${created} / 唤醒 ${started} / 已在跑 ${skipped}（合计 ${n}）"
  sleep 2
  do_status
}

do_status() {
  local p n ok=0 cfg=0
  n=$(farm_port_count "$MOCKS")
  [ "$n" -gt 0 ] || v88_die "MOCKS=${MOCKS} 展开后一个口都没有"
  for p in $(farm_local_ports "$MOCKS"); do
    if curl -fsS -m 3 "http://127.0.0.1:${p}/health" 2>/dev/null | grep -q ok; then
      ok=$((ok + 1))
    else
      v88_warn ":${p} /health 不通"
    fi
    if [ -n "$(v88_farm_read_batch "127.0.0.1:${p}")" ]; then
      cfg=$((cfg + 1))
    else
      v88_warn ":${p} /_config 读不到 batch"
    fi
  done
  v88_log "农场健康 ${ok}/${n}，_config ${cfg}/${n}"
  [ "$ok" -eq "$n" ] && [ "$cfg" -eq "$n" ] || \
    v88_die "农场未齐（health ${ok}/${n}，_config ${cfg}/${n}）。缺的口先修再压，不要把 MOCKS 改小来绕过。"
}

expect_eq() {
  local got="$1" want="$2" label="$3"
  if [ "$got" = "$want" ]; then
    printf '✓ %s → %s\n' "$label" "$got"
  else
    printf '✗ %s → 得 %s 要 %s\n' "$label" "$got" "$want" >&2
    return 1
  fi
}

do_selftest() {
  local err=0 n yaml
  MOCKS=8081-8116
  n=$(farm_port_count "$MOCKS")
  expect_eq "$n" 36 "MOCKS=8081-8116" || err=1
  MOCKS=8081-8112
  n=$(farm_port_count "$MOCKS")
  expect_eq "$n" 32 "MOCKS=8081-8112" || err=1
  MOCKS=8081-8104
  n=$(farm_port_count "$MOCKS")
  expect_eq "$n" 24 "MOCKS=8081-8104" || err=1
  MOCKS=10.56.88.24:8081-8116
  n=$(farm_port_count "$MOCKS")
  expect_eq "$n" 36 "MOCKS=10.56.88.24:8081-8116" || err=1
  MOCK_FIRST=8081
  MOCK_LAST=8112
  MOCKS="${MOCK_FIRST}-${MOCK_LAST}"
  n=$(farm_port_count "$MOCKS")
  expect_eq "$n" 32 "MOCK_FIRST=8081 MOCK_LAST=8112" || err=1
  MOCKS=8081-8112
  MOCK_HOST=10.56.88.29
  yaml=$(emit_catalog)
  n=$(printf '%s\n' "$yaml" | grep -c 'name: mock-' || true)
  expect_eq "$n" 32 "catalog providers MOCKS=8081-8112" || err=1
  n=$(printf '%s\n' "$yaml" | grep -c 'provider: mock-' || true)
  expect_eq "$n" 32 "catalog channels MOCKS=8081-8112" || err=1
  printf '%s\n' "$yaml" | grep -q 'base_url: http://10.56.88.29:8081' || {
    printf '✗ catalog base_url 未指向 MOCK_HOST\n' >&2
    err=1
  }
  if ( farm_local_ports "10.0.0.2:8081-8082,10.0.0.3:8081-8082" ) >/dev/null 2>&1; then
    printf '✗ 多主机 MOCKS 应当拒绝\n' >&2
    err=1
  else
    printf '✓ 多主机 MOCKS 拒绝\n'
  fi
  if ( unset MOCKS MOCK_FIRST MOCK_LAST; resolve_mocks ) >/dev/null 2>&1; then
    printf '✗ 未设 MOCKS 应当拒绝\n' >&2
    err=1
  else
    printf '✓ 未设 MOCKS 拒绝\n'
  fi
  [ "$err" -eq 0 ] || v88_die "start_farm.sh --self-test 未通过"
  v88_log "self-test：口数展开与 catalog 生成通过"
}

case "$CMD" in
  selftest) do_selftest ;;
  *)
    resolve_mocks
    case "$CMD" in
      start) do_start ;;
      status) do_status ;;
      catalog) emit_catalog ;;
    esac
    ;;
esac
