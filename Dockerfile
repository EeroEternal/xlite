# xlite gateway image. Build from the repo root:
#   docker build -t xlite:dev .
#
# **默认面向国内构建**：npm / crates.io（在线分支）/ rustup 三处镜像站内置为默认值，
# `docker build .` 即国内可构建；要回官方源给对应 build-arg 传**空值**。
#   1. Rust 工具链：base 镜像默认**与 `rust-toolchain.toml` 的 pin 同版本**（1.93.1），
#      工具链本体不用下；但官方 rust 镜像是 **minimal profile**（不带 clippy/rustfmt），
#      而 rust-toolchain.toml 要求这两个组件 ⇒ **每次构建 rustup 都会补装**（实测），
#      默认走 USTC 镜像 —— 这正是国内构建原本卡死在 static.rust-lang.org 的那一步。
#      镜像源里没有这个 tag 时 --build-arg 覆盖 base（不同版本同理走镜像）：
#        docker build --build-arg RUST_IMAGE=rust:1.93-bookworm .
#   2. 依赖：构建上下文里**有**本机离线脚手架（`.cargo/` + `vendor/`，用
#      `scripts/prepare_offline_vendor.sh` 生成）时，builder 段走 `cargo build --offline`，
#      不访问 crates.io（rustup 组件仍走 RUSTUP_MIRROR，见第 1 条）；**没有就自动退回
#      在线构建**，crates.io 默认走 rsproxy（回官方：`--build-arg CRATES_MIRROR=`）。这两个目录在 `.gitignore` 里
#      （本地脚手架，从不提交），所以新克隆的机器默认走在线。
#   3. 基础镜像在 docker.io：这是 **daemon 层**的事，Dockerfile 管不了 —— 国内给 dockerd
#      配 registry mirror（/etc/docker/daemon.json 的 "registry-mirrors"，改完
#      systemctl restart docker）。镜像站时效变化频繁，失效就换一个。
#   npm 默认走 npmmirror（回官方：`--build-arg NPM_REGISTRY=`）。
#
# 需要 BuildKit：builder 段用 `RUN --mount=type=bind` 把（被 .dockerignore 过滤后的）
# 整个构建上下文只读挂进容器，才能对**可能不存在**的脚手架做条件分支 —— `COPY` 对不存在
# 的源是硬失败，这正是改动前「新克隆 → docker build」走不通的原因。Docker 23+ / Compose v2 默认启用。
#
# The binary serves Admin from ./admin/dist relative to the process cwd.

ARG RUST_IMAGE=rust:1.93.1-bookworm

FROM node:20-bookworm-slim AS admin
WORKDIR /admin
COPY admin/package.json admin/package-lock.json ./
# npm 默认走 npmmirror；回官方传空：--build-arg NPM_REGISTRY=
ARG NPM_REGISTRY=https://registry.npmmirror.com
RUN if [ -n "$NPM_REGISTRY" ]; then npm config set registry "$NPM_REGISTRY"; fi \
    && npm ci
COPY admin/ ./
RUN npm run build

FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
COPY migrations ./migrations
COPY --from=admin /admin/dist ./admin/dist
# 镜像站默认值（**默认国内**；传空回官方源，如 --build-arg CRATES_MIRROR= ）：
#   RUSTUP_MIRROR — rustup 补 clippy/rustfmt 组件（base 是 minimal profile，每次构建必走）、
#                   及工具链下载（仅 base 与 pin 不同版本时）的镜像。USTC 的 dist 与上游
#                   字节一致；TUNA/rsproxy 的 dist 路径实测 404，别换过去。
#   CRATES_MIRROR — 在线分支的 crates.io sparse 镜像；离线脚手架存在时被忽略（离线优先）。
ARG RUSTUP_MIRROR=https://mirrors.ustc.edu.cn/rust-static
ARG CRATES_MIRROR=sparse+https://rsproxy.cn/index/
# `.cargo/` + `vendor/` 是可选的离线脚手架：本机产物、不进 git（见 .gitignore 与
# scripts/prepare_offline_vendor.sh）。新克隆没有它们，而 `COPY` 不能容忍源缺失，
# 所以从整个（已被 .dockerignore 过滤的）构建上下文只读挂载后，在这里做条件分支。
RUN --mount=type=bind,target=/ctx,ro \
    if [ -n "$RUSTUP_MIRROR" ]; then \
      export RUSTUP_DIST_SERVER="$RUSTUP_MIRROR" RUSTUP_UPDATE_ROOT="$RUSTUP_MIRROR/rustup"; \
    fi; \
    if [ -f /ctx/.cargo/config.toml ] && [ -d /ctx/vendor ] && [ -n "$(ls -A /ctx/vendor)" ]; then \
      echo ">> offline build: using .cargo/ + vendor/ from the build context"; \
      cp -a /ctx/.cargo /ctx/vendor /src/; \
      cargo build --release --bin xlite --offline; \
    else \
      if [ -n "$CRATES_MIRROR" ]; then \
        echo ">> online build via crates mirror: $CRATES_MIRROR"; \
        mkdir -p /src/.cargo \
          && printf '[source.crates-io]\nreplace-with = "crates-mirror"\n\n[source.crates-mirror]\nregistry = "%s"\n' \
             "$CRATES_MIRROR" > /src/.cargo/config.toml; \
      else \
        echo ">> online build: no .cargo/ + vendor/ in the build context; fetching from crates.io"; \
      fi; \
      cargo build --release --bin xlite; \
    fi

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /opt/xlite
COPY --from=builder /src/target/release/xlite /usr/local/bin/xlite
COPY --from=admin /admin/dist ./admin/dist
COPY examples ./examples
ENV PORT=8080 \
    LISTEN_HOST=0.0.0.0 \
    XLITE_UPSTREAM_H2C=1
EXPOSE 8080
ENTRYPOINT ["xlite"]
