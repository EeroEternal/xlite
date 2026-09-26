#!/usr/bin/env bash
# ============================================================
# 给 deploy/v88 这份**自足包**打封条：把「身份决定口径」的文件哈希写进 BUNDLE_SHA256。
#
# 为什么不与仓库比对：现场离线、无 git、无仓库，包必须自足。所以本目录不保留
# sync/跨目录校验那一套（见 README「本目录是自足包」）。
#
# 为什么仍然要封条：§1.1 的真实故障是**文件被拷到现场之后被就地改掉** ——
# 那次 `/root/loadtest/k6/k6.js` 在交付包之外被手改过 ≥4 次，qps 从 1033 跳到 1912、
# TTFT 从 714ms 涨到 1747ms，而日志上看不出来。自封条正是为这一类"拷过去之后被改"兜底。
#
# 用法：deploy/v88/seal_bundle.sh
# 退出码：0 = 已刷新；2 = 有 seal 目标缺失
# ============================================================
set -euo pipefail

V88_DIR=$(cd "$(dirname "$0")" && pwd)
# 只封**身份决定口径**的文件。框架自己的 driver/文档不算：改它们不该需要重新打封条。
SEALED="k6.js advanced/ab_topology.sh lib/lib_rig.sh"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

tmp="$V88_DIR/BUNDLE_SHA256.tmp"
{
  printf '# deploy/v88 自封条 —— 由 seal_bundle.sh 生成，不要手改\n'
  printf '# sealed_at: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if git -C "$V88_DIR" rev-parse --short HEAD >/dev/null 2>&1; then
    printf '# from_commit: %s（记录用，不参与校验）\n' "$(git -C "$V88_DIR" rev-parse --short HEAD 2>/dev/null)"
  fi
  for f in $SEALED; do
    if [ ! -f "$V88_DIR/$f" ]; then
      printf '✗ 缺文件 %s\n' "$V88_DIR/$f" >&2
      rm -f "$tmp"
      exit 2
    fi
    printf '%s  %s\n' "$(sha256_of "$V88_DIR/$f")" "$f"
  done
} >"$tmp"
mv -f "$tmp" "$V88_DIR/BUNDLE_SHA256"

echo "封条已刷新（BUNDLE_SHA256）："
grep -v '^#' "$V88_DIR/BUNDLE_SHA256" | while read -r h n; do printf '  %-18s %s\n' "$n" "$h"; done
