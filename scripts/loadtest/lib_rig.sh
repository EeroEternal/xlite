#!/bin/bash
# =============================================
# 台架互斥锁 —— 三个 harness（run_levels / ab_configs / ab_topology）共用。
#
# 为什么必须有：两个 agent session 共用同一批 mock 与网关，**同时压测量出来的就是
# 「谁在抢资源」的函数**（CPU 争用、链路争用、端口撞车），结论直接作废。
#
# 用法（在脚本里 source 后调用；必须在**当前 shell** 调用，锁才活到脚本结束）：
#     . "$(dirname "$0")/lib_rig.sh"
#     lab_lock "ab_topology local" || exit 1
#
# 实现要点：锁绑在 **fd 9** 上。进程正常退出、崩溃、被 `kill -9`，内核都会释放它
# ⇒ **不会留僵尸锁**。这是不用「写 PID 文件」方案的原因（那套方案在进程被杀后会
# 留下假锁，需要人工清理）。
#
# ⚠️ 这是**建议性**锁：只有同样来要这把锁的调用者才会被挡住。所以要么走 harness，
# 要么遵守约定。⚠️ 约定：**驱动压测固定用同一台机** —— 只在某台机上上锁，
# 锁不住从别处起的入口。
#
# ⚠️⚠️ **调用方必须让所有子进程关掉锁 fd**（`cmd ... 9>&- &`）。锁是挂在 fd 上的：
# 继承了这个 fd 的**后台子进程会替调用者继续持有锁**，即使调用者已经退出 ——
# 于是「kill -9 也不留僵尸锁」这句话就不成立了。实测过：父进程死后，继承 fd 的
# 子进程仍使 `flock -n` 探测 BLOCKED。所以要在**启动任何子进程之前**就注册好
# `trap ... EXIT`，并给每个后台子进程加 `9>&-`。
#
# 锁文件内容只是「方便排查是谁在跑」，真正的互斥靠 fd 上的锁：
# 文件里有旧内容不代表被占用。
# =============================================

lab_lock() {
  local tag="${1:-rig}"
  local who="${LAB_SESSION:-$(hostname)-$$}"
  LAB_LOCK=${LAB_LOCK:-$HOME/lab.lock}

  # 非 Linux（macOS 开发机）没有 flock。**告警放行**，不要用错误的诊断把人挡在门外
  # —— run_levels.sh 明确支持 macOS 开发机（ss 缺失记 unknown、K6_RUNNER=local 调试），
  #    互斥只对压测机有意义。
  if ! command -v flock >/dev/null 2>&1; then
    printf '⚠️ 本机没有 flock（非 Linux）：跳过台架互斥。互斥只在压测机上有效。\n' >&2
    return 0
  fi
  # 用 `<>`（读写、**不截断**）。用 `>` 的话，竞争方一进来就把持有者的信息清空，
  # 紧接着 `cat "$LAB_LOCK"` 只会读到空 —— 诊断信息就丢了。
  exec 9<>"$LAB_LOCK" || return 1
  if ! flock -n 9; then
    {
      printf '✗ 台架被占用：%s\n' "$(cat "$LAB_LOCK" 2>/dev/null)"
      printf '  确认无人使用后再删锁文件：rm %s\n' "$LAB_LOCK"
    } >&2
    return 1
  fi
  # 写占用者信息时**必须走 fd 9**，不能 `> "$LAB_LOCK"`（那会重开并截断文件，
  # 别人刚好 cat 的瞬间会读到空内容 —— 实测 lab 上留下过 0 字节的锁文件）
  printf '%s %s %s\n' "$who" "$(date -Is)" "$tag" >&9
  return 0
}

# ---------------------------------------------------------------- mock 端口展开
# `MOCK_PORTS` 支持**两种**写法（`run_levels.sh` 的 `MOCKS` 也是这两种）：
#   "4101-4124"   区间
#   "4101,4102"   逗号列表
# ⚠️ **不要自己写 `seq "${MOCK_PORTS%-*}" "${MOCK_PORTS#*-}"`** —— 那只认区间：
# 逗号列表下两个参数都是整串，`seq` 报 invalid floating point argument 后**返回空**，
# 于是循环一次都不转、计数全是 0，而 `[ "$n_ok" -eq "$n_all" ]` 这种守卫会**空转通过**
# （0 == 0），把「一个字节都没改农场」当成成功（实测复现）。三个 harness 共用本函数。
expand_ports() {
  case "$1" in
    [0-9]*-[0-9]*) seq "${1%-*}" "${1#*-}" ;;
    *) printf '%s' "$1" | tr ',' ' ' ;;
  esac
}

# ---------------------------------------------------------------- 农场原值记账
# 记下"我们动过哪些 mock 口、它们原来的 batch 是多少"，收尾按口还原。
#
# ⚠️ **不要用关联数组**（`declare -A`）：那是 **bash 4+** 的特性，而 macOS 自带的
# `/bin/bash` 是 **3.2** —— `run_levels.sh` 明确支持 macOS 开发机，而且它用的是
# `set -euo pipefail`，`declare -A` 在 3.2 上会**直接终止脚本**（实测退出码 2）。
# 用一条**空格分隔的 `port=batch` 串**就够了：端口/主机串里不会有空格，
# 也就避免了 nameref（同样是 bash 4.3+）之类更花的东西。
#
# 用法（调用方自己持有那个变量，函数只负责追加后回显）：
#     FARM_SAVED=$(farm_remember "$FARM_SAVED" "$port" "$batch")
# 还原：
#     for kv in $FARM_SAVED; do curl -s -X POST "http://${kv%%=*}/_config?batch=${kv#*=}"; done
farm_remember() { printf '%s %s=%s' "${1:-}" "$2" "$3"; }
