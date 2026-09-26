#!/usr/bin/env bash
# 离线构建脚手架（`.cargo/` + `vendor/`）物化脚本。
#
# 背景：仓库根这两个路径**故意不进 git**（`.gitignore` 里那条
# `# Local offline/vendored build scaffolding (never committed)`），所以一台新克隆的机器
# 两者都没有。而 `Dockerfile` 的离线分支与裸 `cargo build --offline` 都依赖它们。原先
# `Dockerfile` 直接写 `COPY .cargo ./.cargo` / `COPY vendor ./vendor`，Docker 的 `COPY`
# 对不存在的源是**硬失败**，于是「新克隆 → docker build」这条路根本走不通；更要命的是那套
# 脚手架此前只是某台笔记本上的本地产物 —— 全仓 `grep -rn 'cargo vendor'` 在改动前**零命中**，
# 没有任何脚本能重建它（只有 `docs/changelog.md` 里的一句描述）。本脚本就是那个缺失的入口。
#
# 需要**一次**外网（crates.io）。跑完之后下面两条都**不再访问网络**：
#   cargo build --release --bin xlite --offline
#   docker build -t xlite:0.5.0 .        # Dockerfile 检测到脚手架就走离线分支
#
# 用法：
#   scripts/prepare_offline_vendor.sh            # 生成 / 刷新
#   scripts/prepare_offline_vendor.sh --check    # 只校验（不联网），不通过即 exit 2
#
# 退出码：0 成功；1 环境/cargo/网络失败；2 校验不通过（fail-closed，与仓库其它门禁一致）。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

CHECK=0
case "${1:-}" in
  --check) CHECK=1 ;;
  "" ) ;;
  -h|--help)
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0 ;;
  *) echo "unknown arg: $1 (want --check)" >&2; exit 2 ;;
esac

say() { printf '\n\033[1;36m== %s ==\033[0m\n' "$*"; }
ok()  { printf '\033[32m✓ %s\033[0m\n' "$*"; }
bad() { printf '\033[31m✗ %s\033[0m\n' "$*" >&2; }
warn(){ printf '\033[33m! %s\033[0m\n' "$*"; }

# 脚手架三要件：配置文件存在且真的指向 vendored-sources、vendor/ 非空。
verify_files() {
  local rc=0
  if [[ ! -f .cargo/config.toml ]]; then
    bad "缺 .cargo/config.toml —— Dockerfile 会退回在线构建（crates.io）"
    rc=1
  elif ! grep -q 'replace-with *= *"vendored-sources"' .cargo/config.toml; then
    bad ".cargo/config.toml 存在但没有 replace-with = \"vendored-sources\" —— cargo 不会读 vendor/"
    rc=1
  elif ! grep -q 'directory *= *"vendor"' .cargo/config.toml; then
    bad ".cargo/config.toml 的 [source.vendored-sources] 没有 directory = \"vendor\""
    rc=1
  fi
  if [[ ! -d vendor ]]; then
    bad "缺 vendor/ —— --offline 与 Dockerfile 离线分支都不可用"
    rc=1
  elif [[ -z "$(ls -A vendor)" ]]; then
    bad "vendor/ 是空目录 —— 空脚手架比没有更坏：它会骗过 -d 判断，然后在 cargo 里炸"
    rc=1
  fi
  return "$rc"
}

# 最强的一条：让 cargo 真的用 vendor/ 把 Cargo.lock 的图解出来（不编译，秒级）。
# 缺任何一个 crate 都会在这里报错，而不是等 5 分钟后在 docker build 里炸。
verify_resolves() {
  if ! command -v cargo >/dev/null 2>&1; then
    warn "本机没有 cargo —— 跳过「用 vendor/ 解析 Cargo.lock」这步（只有文件存在性被校验）"
    return 2
  fi
  if cargo metadata --offline --locked --format-version 1 >/dev/null 2>&1; then
    return 0
  fi
  bad "cargo metadata --offline --locked 失败 —— vendor/ 与 Cargo.lock 不一致（或 .cargo 配置未生效）"
  echo "    逐个排查：cargo metadata --offline --locked" >&2
  return 1
}

if (( CHECK )); then
  say "校验离线脚手架"
  rc=0
  verify_files || rc=1
  set +e; verify_resolves; r=$?; set -e
  (( r == 1 )) && rc=1
  if (( rc != 0 )); then
    echo >&2
    bad "校验不通过。本机没有网络也要构建的话，先在有网的机器上跑：scripts/prepare_offline_vendor.sh"
    exit 2
  fi
  ok "脚手架可用：.cargo/config.toml + vendor/（$(du -sh vendor | cut -f1)）"
  exit 0
fi

command -v cargo >/dev/null 2>&1 || {
  bad "找不到 cargo。离线脚手架必须由 cargo 生成（rustup/mise 装好工具链再来）。"
  exit 1
}

say "生成离线脚手架（会从 crates.io 拉 Cargo.lock 里的全部 crate）"
mkdir -p .cargo

# 不覆盖一份「不是本脚本生成的」配置：那可能有人手写的 registry mirror 等设置，
# 静默冲掉正是本仓反复吃亏的那类失败。先备份。
if [[ -f .cargo/config.toml ]] && ! grep -q 'vendored-sources' .cargo/config.toml; then
  cp .cargo/config.toml .cargo/config.toml.bak
  warn "已有 .cargo/config.toml 不是 vendored 配置，已备份为 .cargo/config.toml.bak"
fi

# 写临时文件再 mv：cargo 失败时不能留下半截 config（那会让下一次构建拿到坏配置）。
tmp="$repo_root/.cargo/config.toml.tmp"
trap 'rm -f "$tmp"' EXIT
cargo vendor --locked >"$tmp"
mv "$tmp" .cargo/config.toml
ok "vendor/ 与 .cargo/config.toml 已就绪"

say "自证：用 vendor/ 离线解析 Cargo.lock"
set +e; verify_resolves; r=$?; set -e
if (( r == 1 )); then
  bad "刚生成的脚手架没通过解析校验 —— 这不该发生，请把上面的 cargo 报错贴进 issue。"
  exit 1
fi
ok "离线解析通过"
echo
ok "完成。下面两条现在都不访问网络："
echo "    cargo build --release --bin xlite --offline"
echo "    docker build -t xlite:0.5.0 ."
echo "  提醒：.cargo/ 与 vendor/ 仍被 .gitignore 排除（$(du -sh vendor | cut -f1)），不要提交。"
