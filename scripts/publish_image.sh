#!/usr/bin/env bash
# Build and push the xlite Docker image to GHCR (or a custom REGISTRY_IMAGE)
# from a local Linux CI runner.
#
# Usage:
#   # 1. Authenticate once on the CI runner:
#   #    echo "$GHCR_TOKEN" | docker login ghcr.io -u eeroeternal --password-stdin
#   # 2. Build & push current Cargo.toml version + :latest (linux/amd64 by default):
#   bash scripts/publish_image.sh
#   # 3. Multi-arch build & push (linux/amd64,linux/arm64):
#   PLATFORMS=linux/amd64,linux/arm64 bash scripts/publish_image.sh
set -euo pipefail

cd "$(dirname "$0")/.."

IMAGE="${REGISTRY_IMAGE:-ghcr.io/eeroeternal/xlite}"
VERSION="${VERSION:-$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml)}"
PLATFORMS="${PLATFORMS:-}"

if [[ -z "$VERSION" ]]; then
  echo "error: failed to read version from Cargo.toml" >&2
  exit 1
fi

echo "==> Publishing ${IMAGE}:${VERSION} and ${IMAGE}:latest"

if [[ -n "$PLATFORMS" ]]; then
  docker buildx create --name xlite-builder --use >/dev/null 2>&1 || docker buildx use xlite-builder
  docker buildx build \
    --platform "$PLATFORMS" \
    -t "${IMAGE}:${VERSION}" \
    -t "${IMAGE}:latest" \
    --push \
    .
else
  docker build \
    -t "${IMAGE}:${VERSION}" \
    -t "${IMAGE}:latest" \
    .
  docker push "${IMAGE}:${VERSION}"
  docker push "${IMAGE}:latest"
fi

echo "✓ Published ${IMAGE}:${VERSION} and ${IMAGE}:latest"
