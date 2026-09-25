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
# 锁文件内容只是「方便排查是谁在跑」，真正的互斥靠 fd 上的锁：
# 文件里有旧内容不代表被占用。
# =============================================

lab_lock() {
  local tag="${1:-rig}"
  local who="${LAB_SESSION:-$(hostname)-$$}"
  LAB_LOCK=${LAB_LOCK:-$HOME/lab.lock}

  # 在当前 shell 打开 fd 9；锁活到 fd 被关闭（即脚本结束）
  exec 9>"$LAB_LOCK" || return 1
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
