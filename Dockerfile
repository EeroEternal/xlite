# xlite gateway image. Build from the repo root:
#   docker build -t xlite:0.1.0 .
#
# The binary serves Admin from ./admin/dist relative to the process cwd.

FROM node:20-bookworm-slim AS admin
WORKDIR /admin
COPY admin/package.json admin/package-lock.json ./
RUN npm ci
COPY admin/ ./
RUN npm run build

FROM rust:bookworm AS builder
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src ./src
COPY migrations ./migrations
COPY --from=admin /admin/dist ./admin/dist
RUN cargo build --release --bin xlite

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
