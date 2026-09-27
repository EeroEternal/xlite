#!/usr/bin/env bash
# 自证：`v88_farm_hosts` 的 MOCKS 写法展开正确（本机 / 跨机 × 区间 / 列表）。
#
# 为什么需要它：把 driver **挪到非农场机器**（不再和 24 个 mock 同机）时**必须**用跨机
# 写法，而 `host:port-range`（`10.0.0.2:8081-8104`）曾经展开成 **0 个口** ——
# 旧实现按「整串形状」选分支，而 `case` 取**第一个**匹配：该串既「以数字开头且含 `-`」
# 又「含 `:`」，被前者抢走后 `expand_ports` 去跑
# `seq "10.0.0.2:8081" "8104"`，报 `invalid floating point argument`，一个口都不输出。
#
# 后果不是静默算错（driver 会在 preflight fail-loud 退出），而是**报错不解释原因**：
# 「MOCKS=… 展开后一个口都没有」听起来像农场没起来，实际是解析器自己的 bug。
#
# 用法：
#   deploy/v88/lib/selftest_farm_hosts.sh
# 退出码：0 = 全过；1 = 有用例失败。
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib_v88.sh
. "$HERE/lib_v88.sh"

fails=0
passes=0

check() { # <MOCKS> <期望口数> <期望首项（空 = 不查）>
  local mocks="$1" want_n="$2" want_first="${3:-}"
  local out n first
  out=$(v88_farm_hosts "$mocks")
  n=$(printf '%s\n' "$out" | grep -c .)
  first=$(printf '%s\n' "$out" | head -1)
  if [ "$n" = "$want_n" ] && { [ -z "$want_first" ] || [ "$first" = "$want_first" ]; }; then
    printf '✓ %-44s → %s 口\n' "$mocks" "$n"
    passes=$((passes + 1))
  else
    printf '✗ %-44s → %s 口 / 首项 %s（期望 %s 口 / 首项 %s）\n' \
      "$mocks" "$n" "${first:-空}" "$want_n" "${want_first:-不查}"
    fails=$((fails + 1))
  fi
}

echo "== v88_farm_hosts 写法自证 =="

# 本机：区间 / 列表 / 单口
check "8081-8104"                        24 "127.0.0.1:8081"
check "8081-8083"                         3 "127.0.0.1:8081"
check "4101,4102"                         2 "127.0.0.1:4101"
check "9999"                              1 "127.0.0.1:9999"

# 跨机：★ 第一条是回归用例（修之前是 0 口）
check "10.56.88.29:8081-8104"            24 "10.56.88.29:8081"
check "10.0.0.2:8081-8084"                4 "10.0.0.2:8081"
check "10.0.0.2:4101,10.0.0.3:4101"       2 "10.0.0.2:4101"
check "10.0.0.2:8081-8082,10.0.0.3:8081-8082" 4 "10.0.0.2:8081"

# 空 / 畸形输入：展开成 0 口（由调用方 fail-loud，不在这里当成错）
check ""                                  0
check "10.0.0.2:"                         0

# 跨机区间的每一项都必须带主机名（旧实现会把整个区间丢掉）
no_host=$(v88_farm_hosts "10.56.88.29:8081-8104" | grep -vc '^10\.56\.88\.29:')
if [ "$no_host" = "0" ]; then
  printf '✓ %-44s → 每一项都带主机名\n' "10.56.88.29:8081-8104"
  passes=$((passes + 1))
else
  printf '✗ %-44s → 有 %s 项没带主机名\n' "10.56.88.29:8081-8104" "$no_host"
  fails=$((fails + 1))
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "全过（$passes 项）"
  exit 0
fi
echo "失败 $fails 项 / 通过 $passes 项"
exit 1
