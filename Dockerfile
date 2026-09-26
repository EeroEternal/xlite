# xlite gateway image. Build from the repo root:
#   docker build -t xlite:dev .
#
# 国内可构建（两条外网依赖都断掉）：
#   1. Rust 工具链：base 镜像默认**与 `rust-toolchain.toml` 的 pin 同版本**（1.93.1），
#      所以 `cargo` 不会再让 rustup 去 static.rust-lang.org 下工具链。
#      镜像源里没有这个 tag 时用 --build-arg 覆盖，例如：
#        docker build --build-arg RUST_IMAGE=rust:1.93-bookworm -t xlite:dev .
#   2. 依赖：构建上下文里**有**本机离线脚手架（`.cargo/` + `vendor/`，用
#      `scripts/prepare_offline_vendor.sh` 生成）时，builder 段走 `cargo build --offline`，
#      不访问 crates.io；**没有就自动退回在线构建**（`cargo build`，需要 crates.io）。
#      这两个目录在 `.gitignore` 里（本地脚手架，从不提交），所以新克隆的机器默认走在线。
#   npm 仍走 registry；要用镜像站：
#        docker build --build-arg NPM_REGISTRY=https://registry.npmmirror.com -t xlite:dev .
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
ARG NPM_REGISTRY=
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
# `.cargo/` + `vendor/` 是可选的离线脚手架：本机产物、不进 git（见 .gitignore 与
# scripts/prepare_offline_vendor.sh）。新克隆没有它们，而 `COPY` 不能容忍源缺失，
# 所以从整个（已被 .dockerignore 过滤的）构建上下文只读挂载后，在这里做条件分支。
RUN --mount=type=bind,target=/ctx,ro \
    if [ -f /ctx/.cargo/config.toml ] && [ -d /ctx/vendor ] && [ -n "$(ls -A /ctx/vendor)" ]; then \
      echo ">> offline build: using .cargo/ + vendor/ from the build context"; \
      cp -a /ctx/.cargo /ctx/vendor /src/; \
      cargo build --release --bin xlite --offline; \
    else \
      echo ">> online build: no .cargo/ + vendor/ in the build context; fetching from crates.io"; \
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
