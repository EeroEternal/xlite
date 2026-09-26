# xlite gateway image. Build from the repo root:
#   docker build -t xlite:dev .
#
# 国内可构建（两条外网依赖都断掉）：
#   1. Rust 工具链：base 镜像默认**与 `rust-toolchain.toml` 的 pin 同版本**（1.93.1），
#      所以 `cargo` 不会再让 rustup 去 static.rust-lang.org 下工具链。
#      镜像源里没有这个 tag 时用 --build-arg 覆盖，例如：
#        docker build --build-arg RUST_IMAGE=rust:1.93-bookworm -t xlite:dev .
#   2. 依赖：镜像内拷 `.cargo/` + `vendor/` 并 `cargo build --offline`，不访问 crates.io。
#   npm 仍走 registry；要用镜像站：
#        docker build --build-arg NPM_REGISTRY=https://registry.npmmirror.com -t xlite:dev .
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
COPY .cargo ./.cargo
COPY vendor ./vendor
COPY crates ./crates
COPY src ./src
COPY migrations ./migrations
COPY --from=admin /admin/dist ./admin/dist
RUN cargo build --release --bin xlite --offline

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
